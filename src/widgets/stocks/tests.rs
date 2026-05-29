// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the stocks widget. Split out of `mod.rs` per the repo standard.

use super::*;

fn build_widget(cfg: StocksConfig) -> StocksWidget {
    StocksWidget::with_config(
        "main".to_string(),
        cfg,
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    )
}

fn quote(symbol: &str, price: f64, prev: f64) -> StockQuote {
    StockQuote {
        symbol: symbol.into(),
        short_name: symbol.into(),
        price,
        previous_close: prev,
        day_high: None,
        day_low: None,
        fifty_two_week_high: None,
        fifty_two_week_low: None,
        volume: None,
        avg_volume: None,
        market_cap: None,
        shares_outstanding: None,
        pe_ratio: None,
        eps: None,
        dividend_yield: None,
        beta: None,
        currency: None,
        intraday: vec![],
        intraday_timestamps: vec![],
        regular_session_start_ts: None,
        regular_session_end_ts: None,
        previous_session_start_ts: None,
        previous_session_end_ts: None,
        fetched_at: chrono::Local::now(),
        post_market_price: None,
        post_market_change: None,
        post_market_change_percent: None,
        pre_market_price: None,
        pre_market_change: None,
        pre_market_change_percent: None,
        market_state: None,
    }
}

#[test]
fn quotes_buckets_are_isolated_per_period() {
    // Per-period buckets must keep their data when switching back
    // and forth. If a write to one bucket leaks into another the
    // user sees the wrong period's bars under the new x-axis
    // labels (the bug that motivated the per-period split).
    let mut st = StocksState::default();
    let day_q = quote("AAPL", 200.0, 199.0);
    st.quotes_mut(Period::Day)
        .insert("AAPL".into(), QuoteState::Ready(Arc::new(day_q.clone())));
    assert_eq!(st.quotes(Period::Day).len(), 1);
    assert_eq!(
        st.quotes(Period::Week).len(),
        0,
        "Week bucket should not see Day's data"
    );

    let week_q = quote("AAPL", 210.0, 200.0);
    st.quotes_mut(Period::Week)
        .insert("AAPL".into(), QuoteState::Ready(Arc::new(week_q.clone())));
    assert_eq!(st.quotes(Period::Day).len(), 1);
    assert_eq!(st.quotes(Period::Week).len(), 1);

    // The Day quote remains intact after the Week insert (no
    // cross-contamination).
    match st.quotes(Period::Day).get("AAPL") {
        Some(QuoteState::Ready(q)) => {
            assert_eq!(q.price, 200.0, "Day price should still be 200.0");
        }
        _ => panic!("Day bucket should still hold a Ready entry"),
    }
    match st.quotes(Period::Week).get("AAPL") {
        Some(QuoteState::Ready(q)) => {
            assert_eq!(q.price, 210.0);
        }
        _ => panic!("Week bucket should hold a Ready entry"),
    }
}

#[test]
fn humanize_big_uses_expected_suffixes() {
    assert_eq!(humanize_big(500.0), "500");
    assert_eq!(humanize_big(15_400.0), "15.4K");
    assert_eq!(humanize_big(48_200_000.0), "48.20M");
    assert_eq!(humanize_big(3_020_000_000_000.0), "3.02T");
}

#[test]
fn all_symbols_orders_indices_first_then_watchlist() {
    let w = build_widget(StocksConfig::default());
    let syms = w.all_symbols();
    assert_eq!(syms[0], "^DJI");
    assert_eq!(syms[3], "AAPL");
}

/// `:stock AAPL` against a watchlist that already contains AAPL
/// used to set `transient_ticker = Some("AAPL")` + `selected =
/// base_slot`, but `all_symbols()` dedupes the transient against
/// the watchlist — so selection landed past the visible list and
/// the graph stayed blank. Snap to the existing row instead.
#[test]
fn lookup_existing_uppercase_ticker_jumps_to_watchlist_row() {
    let w = build_widget(StocksConfig::default());
    // AAPL sits at index 3 (3 indices + AAPL at watchlist[0]).
    w.lookup_and_set_transient("AAPL");
    let st = w.state.lock().unwrap();
    assert_eq!(st.selected, 3);
    assert!(st.transient_ticker.is_none(), "no transient pin needed");
}

/// Same fast path, lower-cased. The case-insensitive match
/// catches `:stock aapl` typed by muscle memory.
#[test]
fn lookup_existing_lowercase_ticker_jumps_to_watchlist_row() {
    let w = build_widget(StocksConfig::default());
    w.lookup_and_set_transient("aapl");
    let st = w.state.lock().unwrap();
    assert_eq!(st.selected, 3);
    assert!(st.transient_ticker.is_none());
}

/// Indices count too: `:stock ^GSPC` should land on the S&P row,
/// not pin a duplicate transient. ^GSPC is indices[1].
#[test]
fn lookup_existing_index_symbol_jumps_to_index_row() {
    let w = build_widget(StocksConfig::default());
    w.lookup_and_set_transient("^GSPC");
    let st = w.state.lock().unwrap();
    assert_eq!(st.selected, 1);
    assert!(st.transient_ticker.is_none());
}

/// Already-pinned transient gets snapped to instead of re-pinned.
/// User pins SPY (transient row appears at the end), then types
/// `:stock spy` again — selection should land on the existing
/// transient row.
#[test]
fn lookup_existing_transient_jumps_to_transient_row() {
    let w = build_widget(StocksConfig::default());
    w.lookup_and_set_transient("SPY");
    let base_slot = w.config.indices.len() + w.config.watchlist.len();
    {
        let st = w.state.lock().unwrap();
        assert_eq!(st.selected, base_slot);
        assert_eq!(st.transient_ticker.as_deref(), Some("SPY"));
    }
    // Second invocation — same symbol, different case. Should
    // snap to the existing transient row, not re-pin it.
    w.lookup_and_set_transient("spy");
    let st = w.state.lock().unwrap();
    assert_eq!(st.selected, base_slot);
    assert_eq!(
        st.transient_ticker.as_deref(),
        Some("SPY"),
        "transient should still be SPY, not re-pinned"
    );
}

/// New ticker that isn't on screen → pin it as transient at
/// `base_slot`, same as the original behavior. Guards against the
/// fast-path swallowing legitimate new-ticker lookups.
#[test]
fn lookup_new_ticker_pins_transient_at_base_slot() {
    let w = build_widget(StocksConfig::default());
    let base_slot = w.config.indices.len() + w.config.watchlist.len();
    w.lookup_and_set_transient("BRK-A");
    let st = w.state.lock().unwrap();
    assert_eq!(st.selected, base_slot);
    assert_eq!(st.transient_ticker.as_deref(), Some("BRK-A"));
}

#[test]
fn cycle_period_wraps_at_both_ends() {
    let mut w = build_widget(StocksConfig::default());
    assert_eq!(w.period, Period::Day);
    w.cycle_period(true);
    assert_eq!(w.period, Period::Week);
    // Walk forward to the last variant, then once more to wrap to first.
    for _ in 0..Period::ALL.len() - 2 {
        w.cycle_period(true);
    }
    assert_eq!(w.period, Period::TenYear);
    w.cycle_period(true);
    assert_eq!(w.period, Period::Day);
    // Backward wraps too.
    w.cycle_period(false);
    assert_eq!(w.period, Period::TenYear);
}

#[test]
fn move_selection_clamps() {
    let mut w = build_widget(StocksConfig::default());
    w.move_selection(-5);
    assert_eq!(w.state.lock().unwrap().selected, 0);
    w.move_selection(100);
    let total = w.all_symbols().len() - 1;
    assert_eq!(w.state.lock().unwrap().selected, total);
}

#[test]
fn label_rows_spans_full_plot_height() {
    // Tall plot → 5 labels at top/¼/mid/¾/bottom.
    let rows = label_rows(8);
    assert_eq!(rows.first(), Some(&0));
    assert_eq!(rows.last(), Some(&7));
    assert!(rows.len() >= 3);
    // Medium plot → 3 labels.
    let rows = label_rows(5);
    assert_eq!(rows.first(), Some(&0));
    assert_eq!(rows.last(), Some(&4));
    // Short plot → top + bottom only.
    let rows = label_rows(3);
    assert_eq!(rows, vec![0, 2]);
    // Single-row plot.
    assert_eq!(label_rows(1), vec![0]);
    assert!(label_rows(0).is_empty());
}

#[test]
fn list_row_includes_arrow_for_selected_only() {
    let q = quote("AAPL", 200.0, 196.0);
    let qs: HashMap<String, QuoteState> = {
        let mut m = HashMap::new();
        m.insert("AAPL".to_string(), QuoteState::Ready(Arc::new(q)));
        m
    };
    let line_sel = format_list_row(
        "AAPL",
        qs.get("AAPL"),
        true,
        DisplayMode::Percent,
        Period::Day,
    );
    let line_un = format_list_row(
        "AAPL",
        qs.get("AAPL"),
        false,
        DisplayMode::Percent,
        Period::Day,
    );
    let sel_text: String = line_sel.spans.iter().map(|s| s.content.as_ref()).collect();
    let un_text: String = line_un.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(sel_text.contains("▸"));
    assert!(!un_text.contains("▸"));
}

/// Construct a `DateTime<Utc>` for a given America/New_York local
/// timestamp. Centralized so each market-hours test reads cleanly.
fn et_to_utc(y: i32, m: u32, d: u32, hour: u32, minute: u32) -> chrono::DateTime<Utc> {
    let tz: Tz = MARKET_TZ.parse().unwrap();
    let local = tz
        .with_ymd_and_hms(y, m, d, hour, minute, 0)
        .single()
        .expect("valid local time");
    local.with_timezone(&Utc)
}

#[test]
fn extended_market_hours_covers_premarket_through_postmarket() {
    // 04:00 ET Monday → in window.
    assert!(is_extended_market_hours(et_to_utc(2026, 5, 18, 4, 0)));
    // 09:30 ET (regular open) → in window.
    assert!(is_extended_market_hours(et_to_utc(2026, 5, 18, 9, 30)));
    // 19:59 ET → still in window (post-market closes at 20:00).
    assert!(is_extended_market_hours(et_to_utc(2026, 5, 18, 19, 59)));
    // 20:00 ET → out of window.
    assert!(!is_extended_market_hours(et_to_utc(2026, 5, 18, 20, 0)));
    // 03:00 ET → before pre-market open.
    assert!(!is_extended_market_hours(et_to_utc(2026, 5, 18, 3, 0)));
    // Saturday → never in window even during pre/post hours.
    assert!(!is_extended_market_hours(et_to_utc(2026, 5, 16, 10, 0)));
}

#[test]
fn extended_hours_segment_meta_fallback_prefers_post_after_close() {
    // No bars/timestamps populated → falls back to Yahoo's meta fields.
    let mut q = quote("AAPL", 200.0, 196.0);
    q.market_state = Some("POST".into());
    q.post_market_change = Some(0.42);
    q.post_market_change_percent = Some(0.21);
    let seg = extended_hours_segment(&q).expect("AH segment present");
    assert_eq!(seg.0, "AH");
    assert!((seg.1 - 0.42).abs() < 1e-9);
}

#[test]
fn extended_hours_segment_meta_fallback_uses_pre_in_morning() {
    let mut q = quote("AAPL", 200.0, 196.0);
    q.market_state = Some("PRE".into());
    q.pre_market_change = Some(-0.15);
    q.pre_market_change_percent = Some(-0.08);
    let seg = extended_hours_segment(&q).expect("PRE segment present");
    assert_eq!(seg.0, "PRE");
    assert!(seg.1 < 0.0);
}

#[test]
fn extended_hours_segment_hidden_when_change_is_zero() {
    let mut q = quote("AAPL", 200.0, 196.0);
    q.market_state = Some("POST".into());
    q.post_market_change = Some(0.0);
    q.post_market_change_percent = Some(0.0);
    assert!(extended_hours_segment(&q).is_none());
}

#[test]
fn extended_hours_from_bars_shows_ah_when_latest_bar_is_post_market() {
    // 4:10pm ET scenario: regular session just closed at 196.0
    // (q.price = official close auction), latest AH bar is at 198.0.
    // Expect AH = +2.00 (+1.02%) against the official close, not
    // against any boundary bar's close.
    let mut q = quote("AAPL", 196.0, 195.0); // q.price = today's close, prev = yesterday
    q.regular_session_start_ts = Some(1_700_000_000); // anchor
    q.regular_session_end_ts = Some(1_700_023_400); // anchor + 6.5h
    q.intraday_timestamps = vec![
        1_700_023_100, // 15:55 — last regular bar
        1_700_023_400, // 16:00 — boundary (first AH bar in Yahoo's convention)
        1_700_023_700, // 16:05 AH
        1_700_024_000, // 16:10 AH
    ];
    // Boundary bar's close differs from official close — that's the
    // whole point. We use q.price, not the boundary bar.
    q.intraday = vec![195.93, 196.07, 197.0, 198.0];
    let seg = extended_hours_segment(&q).expect("AH from bars");
    assert_eq!(seg.0, "AH");
    assert!((seg.1 - 2.0).abs() < 1e-9, "got {}", seg.1);
    assert!((seg.2 - (2.0 / 196.0 * 100.0)).abs() < 1e-6);
}

#[test]
fn extended_hours_from_bars_persists_after_post_market_closes() {
    // 21:22 ET scenario: post-market closed at 20:00 ET, no new bars
    // are coming. Last bar is past reg_end → still AH against
    // today's close.
    let mut q = quote("AAPL", 196.0, 195.0);
    q.regular_session_start_ts = Some(1_700_000_000);
    q.regular_session_end_ts = Some(1_700_023_400);
    q.intraday_timestamps = vec![1_700_023_400, 1_700_037_800];
    q.intraday = vec![196.07, 198.0];
    let seg = extended_hours_segment(&q).expect("AH persists overnight");
    assert_eq!(seg.0, "AH");
    assert!((seg.1 - 2.0).abs() < 1e-9);
}

#[test]
fn extended_hours_from_bars_shows_pre_when_latest_bar_is_premarket() {
    // Next morning: pre-market bars exist, no regular bars yet.
    // q.price still reflects yesterday's close (Yahoo updates it
    // at the close auction and holds until next open).
    let mut q = quote("AAPL", 196.0, 195.0); // q.price = yesterday's close
    q.regular_session_start_ts = Some(1_700_100_000);
    q.regular_session_end_ts = Some(1_700_123_400);
    // Bars at ~06:00 ET (3.5h before today's open — well within
    // PRE_MARKET_LOOKBACK_SECS of 7h).
    q.intraday_timestamps = vec![1_700_086_000, 1_700_087_800];
    q.intraday = vec![198.0, 199.0];
    let seg = extended_hours_segment(&q).expect("PRE from bars");
    assert_eq!(seg.0, "PRE");
    // chg = 199 (last PRE bar) - 196 (yesterday's close) = +3.
    assert!((seg.1 - 3.0).abs() < 1e-9, "got {}", seg.1);
}

#[test]
fn extended_hours_from_bars_shows_ah_in_overnight_gap() {
    // The 2am-ET case. Today's regular session hasn't started; the
    // latest bar is yesterday's last AH bar from ~7:30pm. The old
    // logic mis-labeled this PRE and computed chg against Yahoo's
    // `chartPreviousClose` (= the day-before-yesterday's close).
    // Now it labels AH against q.price (= yesterday's official
    // close auction).
    let mut q = quote("AAPL", 196.0, 100.0); // previous_close junk to prove
                                              // we don't depend on it.
    // Today's reg session: anchor (~09:30 ET) to anchor + 6.5h.
    q.regular_session_start_ts = Some(1_700_100_000);
    q.regular_session_end_ts = Some(1_700_123_400);
    // Yesterday's last AH bar is well before today's reg_start —
    // ~16h before, outside the 7h PRE_MARKET_LOOKBACK window.
    q.intraday_timestamps = vec![
        1_700_037_000, // yesterday boundary bar
        1_700_040_000, // yesterday AH
        1_700_044_000, // yesterday last AH (~7:30 ET)
    ];
    q.intraday = vec![196.07, 197.0, 198.0];
    let seg = extended_hours_segment(&q).expect("AH from overnight gap");
    assert_eq!(seg.0, "AH");
    // chg = 198 - 196 (q.price).
    assert!((seg.1 - 2.0).abs() < 1e-9, "got {}", seg.1);
    assert!((seg.2 - (2.0 / 196.0 * 100.0)).abs() < 1e-6);
}

#[test]
fn extended_hours_ignores_boundary_bar_for_baseline() {
    // Regression test for the bug the user found by cross-checking
    // against MarketWatch: Yahoo's bar at ts == reg_end is the
    // FIRST AH bar, not the regular-close bar, and its close
    // differs from the official close auction price by a few
    // cents. We must use q.price (the auction price), not any
    // bar's close, as the AH baseline. With real AAPL boundary
    // data: close auction 310.85, boundary bar 310.92, last AH
    // 310.60. MW shows -$0.25 (= 310.60 - 310.85); the old logic
    // computed -$0.32 against the boundary bar.
    let mut q = quote("AAPL", 310.85, 308.33);
    q.regular_session_start_ts = Some(1_700_000_000);
    q.regular_session_end_ts = Some(1_700_023_400);
    q.intraday_timestamps = vec![
        1_700_023_100, // 15:55 last regular bar
        1_700_023_400, // 16:00 boundary (close = 310.92 ≠ 310.85 auction)
        1_700_024_300, // 16:15 AH
        1_700_037_800, // last AH (~20:00, 4h after close)
    ];
    q.intraday = vec![310.93, 310.92, 310.71, 310.60];
    let seg = extended_hours_segment(&q).expect("AH segment present");
    assert_eq!(seg.0, "AH");
    let expected = 310.60 - 310.85;
    assert!(
        (seg.1 - expected).abs() < 1e-6,
        "expected {expected}, got {}",
        seg.1
    );
}

#[test]
fn period_annotations_1d_returns_empty() {
    // 1D suppresses annotation-driven labels (the regular session is a
    // single uniform block; the legacy 9:30/10:45/... labels work fine).
    let ts: Vec<i64> = (0..78).map(|i| 1_700_000_000 + i * 300).collect();
    assert!(period_annotations(Period::Day, &ts).is_empty());
}

#[test]
fn period_annotations_six_month_marks_month_boundaries() {
    // Use mid-month noon-UTC timestamps to dodge TZ-boundary ambiguity:
    // mid-Mar / mid-Apr / mid-May land in their named month in any
    // local TZ, so we should see three distinct month annotations.
    let ts = vec![
        1_773_172_800, // 2026-03-15 12:00 UTC
        1_775_851_200, // 2026-04-15 12:00 UTC
        1_778_443_200, // 2026-05-15 12:00 UTC
    ];
    let anns = period_annotations(Period::SixMonth, &ts);
    assert_eq!(anns.len(), 3);
    for ann in &anns {
        assert_eq!(ann.label.len(), 3, "label {:?}", ann.label);
    }
}

#[test]
fn period_annotations_year_uses_quarter_boundaries() {
    // Months 1, 2, 3 (same quarter), 4 (new quarter), 7 (new quarter)
    // — expect 3 annotations (Q1, Q2, Q3).
    let ts = vec![
        1_767_225_600, // Jan 1 2026
        1_769_904_000, // Feb 1 2026
        1_772_323_200, // Mar 1 2026
        1_775_001_600, // Apr 1 2026
        1_783_036_800, // Jul 1 2026
    ];
    let anns = period_annotations(Period::Year, &ts);
    assert_eq!(anns.len(), 3);
}

#[test]
fn period_annotations_five_year_uses_year_boundaries() {
    // Jan 1 of 2022, 2023, 2024, 2025, 2026.
    let ts = vec![
        1_640_995_200, // 2022-01-01 UTC
        1_672_531_200, // 2023-01-01 UTC
        1_704_067_200, // 2024-01-01 UTC
        1_735_689_600, // 2025-01-01 UTC
        1_767_225_600, // 2026-01-01 UTC
    ];
    let anns = period_annotations(Period::FiveYear, &ts);
    assert_eq!(anns.len(), 5);
}

#[test]
fn period_annotations_ten_year_keeps_only_even_years() {
    let ts = vec![
        1_577_836_800, // 2020-01-01 UTC
        1_609_459_200, // 2021-01-01 UTC
        1_640_995_200, // 2022-01-01 UTC
        1_672_531_200, // 2023-01-01 UTC
        1_704_067_200, // 2024-01-01 UTC
    ];
    let anns = period_annotations(Period::TenYear, &ts);
    let years: Vec<i32> = anns.iter().filter_map(|a| a.label.parse().ok()).collect();
    assert!(years.iter().all(|y| y % 2 == 0), "got {:?}", years);
}

#[test]
fn lay_out_x_axis_labels_at_cols_places_labels_around_target_columns() {
    // 30-wide line, three labels at cols 0, 14, 29. The middle label
    // is centered: a 3-char label centered on col 14 occupies cols
    // 13..16 (left = col - len/2).
    let items = vec![(0, "Jan"), (14, "May"), (29, "Dec")];
    let line = lay_out_x_axis_labels_at_cols(&items, 30);
    assert_eq!(line.len(), 30);
    assert!(line.starts_with("Jan"));
    assert!(line.ends_with("Dec"));
    let chars: Vec<char> = line.chars().collect();
    let mid: String = chars[13..16].iter().collect();
    assert_eq!(mid, "May");
}

#[test]
fn lay_out_x_axis_labels_at_cols_skips_overlaps() {
    // Two labels too close to fit — second one collides with the first
    // and gets dropped.
    let items = vec![(0, "Jan"), (1, "Feb")];
    let line = lay_out_x_axis_labels_at_cols(&items, 10);
    assert!(line.starts_with("Jan"));
    assert!(!line.contains("Feb"));
}

#[test]
fn pick_day_chart_bars_uses_today_regular_when_present() {
    // Mid-regular-session: bars exist within today's reg range.
    // Chart should use today's regular bars only.
    let mut q = quote("AAPL", 200.0, 195.0);
    q.regular_session_start_ts = Some(1_700_000_000);
    q.regular_session_end_ts = Some(1_700_023_400);
    q.previous_session_start_ts = Some(1_699_900_000);
    q.previous_session_end_ts = Some(1_699_923_400);
    q.intraday_timestamps = vec![
        1_699_920_000, // yesterday regular (would be in prev range)
        1_700_005_000, // today regular
        1_700_010_000, // today regular
    ];
    q.intraday = vec![196.0, 199.0, 200.0];
    let (vs, ts) = pick_day_chart_bars(&q).expect("today's regular session present");
    assert_eq!(ts, vec![1_700_005_000, 1_700_010_000]);
    assert_eq!(vs, vec![199.0, 200.0]);
}

#[test]
fn pick_day_chart_bars_falls_back_to_previous_when_today_empty() {
    // Pre-market on a new day: today's regular range has no bars
    // yet, but yesterday's full session is in the data. Chart
    // should show yesterday's bars (not the pre-market bars).
    let mut q = quote("AAPL", 196.0, 195.0);
    q.regular_session_start_ts = Some(1_700_100_000); // today reg_start (well in future)
    q.regular_session_end_ts = Some(1_700_123_400);
    q.previous_session_start_ts = Some(1_700_000_000);
    q.previous_session_end_ts = Some(1_700_023_400);
    q.intraday_timestamps = vec![
        1_700_005_000, // yesterday regular
        1_700_010_000, // yesterday regular
        1_700_044_000, // yesterday AH (~7:30 ET) — must NOT appear
        1_700_086_000, // today PRE (~6:00 ET) — must NOT appear
        1_700_087_800, // today PRE — must NOT appear
    ];
    q.intraday = vec![196.0, 197.0, 198.0, 198.5, 199.0];
    let (vs, ts) = pick_day_chart_bars(&q).expect("previous session fallback");
    assert_eq!(
        ts,
        vec![1_700_005_000, 1_700_010_000],
        "pre-market and AH bars must be excluded"
    );
    assert_eq!(vs, vec![196.0, 197.0]);
}

#[test]
fn pick_day_chart_bars_excludes_post_market_when_today_has_regular_bars() {
    // After today's regular close: today's regular bars are present
    // alongside today's AH bars. Chart should drop the AH bars.
    let mut q = quote("AAPL", 200.0, 195.0);
    q.regular_session_start_ts = Some(1_700_000_000);
    q.regular_session_end_ts = Some(1_700_023_400);
    q.intraday_timestamps = vec![
        1_700_005_000, // regular
        1_700_023_000, // regular (5min before close)
        1_700_023_400, // boundary (Yahoo: first AH bar) — must NOT appear
        1_700_024_000, // AH — must NOT appear
    ];
    q.intraday = vec![198.0, 200.0, 200.5, 201.0];
    let (vs, ts) = pick_day_chart_bars(&q).expect("today's regular bars");
    // Filter is inclusive on both ends, so boundary bar is included
    // intentionally — keeps the regular-close auction within the
    // chart even when Yahoo timestamps it at ts == reg_end. AH bars
    // after that drop off.
    assert_eq!(ts, vec![1_700_005_000, 1_700_023_000, 1_700_023_400]);
    assert_eq!(vs, vec![198.0, 200.0, 200.5]);
}

#[test]
fn pick_day_chart_bars_returns_none_when_no_session_bounds() {
    // Yahoo didn't return trading periods at all (rare edge case).
    // Helper returns None and the caller falls back to unfiltered
    // data.
    let mut q = quote("AAPL", 196.0, 195.0);
    q.intraday_timestamps = vec![1_700_005_000, 1_700_010_000];
    q.intraday = vec![195.0, 196.0];
    assert!(pick_day_chart_bars(&q).is_none());
}

#[test]
fn extended_hours_from_bars_hidden_during_regular_session() {
    // Latest bar is mid-day regular session — no AH/PRE segment.
    let mut q = quote("AAPL", 197.0, 196.0);
    q.regular_session_start_ts = Some(1_700_000_000);
    q.regular_session_end_ts = Some(1_700_023_400);
    q.intraday_timestamps = vec![1_700_005_000, 1_700_010_000]; // both inside session
    q.intraday = vec![196.5, 197.0];
    assert!(extended_hours_segment(&q).is_none());
}

#[test]
fn market_status_open_counts_down_to_close() {
    // 2026-05-18 is a Monday. 10:30 ET → 5h30m until 16:00 close.
    let now = et_to_utc(2026, 5, 18, 10, 30);
    let s = market_status_line(now);
    assert!(s.is_open);
    assert_eq!(s.message, "5h30m until market close");
}

#[test]
fn market_status_after_hours_counts_down_to_next_open() {
    // Monday 18:00 ET → next open is Tuesday 09:30 ET → 15h30m.
    let now = et_to_utc(2026, 5, 18, 18, 0);
    let s = market_status_line(now);
    assert!(!s.is_open);
    assert_eq!(s.message, "15h30m until market open");
}

#[test]
fn market_status_pre_open_counts_down_to_today_open() {
    // Monday 08:00 ET → today's open at 09:30 ET → 1h30m.
    let now = et_to_utc(2026, 5, 18, 8, 0);
    let s = market_status_line(now);
    assert!(!s.is_open);
    assert_eq!(s.message, "1h30m until market open");
}

#[test]
fn market_status_weekend_skips_to_monday() {
    // Saturday 12:00 ET → Monday 09:30 ET. ~45h30m.
    let now = et_to_utc(2026, 5, 16, 12, 0);
    let s = market_status_line(now);
    assert!(!s.is_open);
    assert!(
        s.message.ends_with("until market open"),
        "got {:?}",
        s.message
    );
    // Saturday 12:00 → Monday 09:30 = 45.5h
    assert!(s.message.starts_with("45h"), "got {:?}", s.message);
}

#[test]
fn format_hm_drops_hours_when_zero() {
    assert_eq!(format_hm(ChronoDuration::minutes(45)), "45m");
    assert_eq!(format_hm(ChronoDuration::minutes(0)), "0m");
    assert_eq!(format_hm(ChronoDuration::seconds(7200 + 65)), "2h1m");
}

fn d(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

#[test]
fn holiday_fixed_date_new_years_and_christmas() {
    assert!(is_market_holiday(d(2026, 1, 1)));
    assert!(is_market_holiday(d(2026, 12, 25)));
}

#[test]
fn holiday_independence_day_observed_friday_when_saturday() {
    // July 4 2026 falls on Saturday → observed Friday Jul 3.
    assert!(is_market_holiday(d(2026, 7, 3)));
    // July 4 itself is the weekend; the observation logic shifts
    // the closure to the 3rd. The 4th alone wouldn't be a trading
    // day either way, so the test is about the observed day catching.
}

#[test]
fn holiday_floating_dates() {
    // MLK = 3rd Mon of Jan 2026 → Jan 19.
    assert!(is_market_holiday(d(2026, 1, 19)));
    // Presidents = 3rd Mon of Feb 2026 → Feb 16.
    assert!(is_market_holiday(d(2026, 2, 16)));
    // Memorial = last Mon of May 2026 → May 25.
    assert!(is_market_holiday(d(2026, 5, 25)));
    // Labor = 1st Mon of Sep 2026 → Sep 7.
    assert!(is_market_holiday(d(2026, 9, 7)));
    // Thanksgiving = 4th Thu of Nov 2026 → Nov 26.
    assert!(is_market_holiday(d(2026, 11, 26)));
}

#[test]
fn holiday_good_friday_via_easter_2026() {
    // Easter 2026 = April 5; Good Friday = April 3.
    assert!(is_market_holiday(d(2026, 4, 3)));
    // April 2 (Thursday) is NOT a holiday.
    assert!(!is_market_holiday(d(2026, 4, 2)));
}

#[test]
fn half_day_black_friday_2026() {
    // Day after Thanksgiving 2026 → Friday Nov 27.
    assert!(is_market_half_day(d(2026, 11, 27)));
    // The Thursday before (Thanksgiving) is a full closure, not half.
    assert!(!is_market_half_day(d(2026, 11, 26)));
}

#[test]
fn half_day_christmas_eve_weekday_only() {
    // 2026: Dec 24 is Thursday → half-day.
    assert!(is_market_half_day(d(2026, 12, 24)));
    // 2027: Dec 24 is Friday → observed Christmas closure (not
    // a half-day per our model).
    assert!(!is_market_half_day(d(2027, 12, 24)));
    // 2028: Dec 24 is Sunday → not a trading day either way.
    assert!(!is_market_half_day(d(2028, 12, 24)));
}

#[test]
fn market_status_skips_full_closure_holidays() {
    // Thursday Nov 26, 2026 is Thanksgiving → countdown points to
    // Friday Nov 27 (the half-day) at 09:30 ET.
    let now = et_to_utc(2026, 11, 26, 11, 0);
    let s = market_status_line(now);
    assert!(!s.is_open);
    assert!(s.message.contains("until market open"));
}

#[test]
fn market_status_uses_early_close_on_half_day() {
    // 2026-11-27 (Black Friday) at 12:00 ET → 1h until 13:00 close.
    let now = et_to_utc(2026, 11, 27, 12, 0);
    let s = market_status_line(now);
    assert!(s.is_open);
    assert_eq!(s.message, "1h0m until market close");
}

#[test]
fn nth_weekday_matches_known_dates() {
    // 3rd Mon of Jan 2026 = Jan 19.
    assert_eq!(nth_weekday(2026, 1, Weekday::Mon, 3), Some(d(2026, 1, 19)));
    // 4th Thu of Nov 2026 = Nov 26.
    assert_eq!(
        nth_weekday(2026, 11, Weekday::Thu, 4),
        Some(d(2026, 11, 26))
    );
}

#[test]
fn last_weekday_handles_december_rollover() {
    // Last Mon of Dec 2026 = Dec 28.
    assert_eq!(last_weekday(2026, 12, Weekday::Mon), Some(d(2026, 12, 28)));
}

#[test]
fn is_leap_year_matches_gregorian_rule() {
    assert!(is_leap_year(2024));
    assert!(!is_leap_year(2025));
    assert!(!is_leap_year(2026));
    assert!(is_leap_year(2000)); // /400 → leap
    assert!(!is_leap_year(1900)); // /100 but not /400 → common
    assert!(is_leap_year(2400));
}

#[test]
fn x_axis_label_layout_anchors_last_label_at_right_edge() {
    // 6 labels into 60 cells: the old `step = plot_w/n` formula
    // placed "now" at col 50, leaving 7 cells of trailing
    // whitespace short of the plot's right edge. The new layout
    // puts "now" so its right edge lands at col 60 (i.e. left
    // edge at col 57).
    let labels = ["-5y", "-4y", "-3y", "-2y", "-1y", "now"];
    let line = lay_out_x_axis_labels(&labels, 60);
    let trimmed = line.trim_end_matches(' ');
    assert!(trimmed.ends_with("now"));
    assert_eq!(line.chars().count(), 60);
    // "now" sits at cols 57..60 (left-edge 57 = (5 * 57) / 5 = 57).
    assert_eq!(&line[line.len() - 3..], "now");
}

#[test]
fn x_axis_label_layout_left_anchors_first_label() {
    let labels = ["Jan", "Mar", "May", "Jul", "Sep", "Nov"];
    let line = lay_out_x_axis_labels(&labels, 60);
    assert!(line.starts_with("Jan"));
}

#[test]
fn x_axis_label_layout_handles_single_label() {
    let labels = ["solo"];
    let line = lay_out_x_axis_labels(&labels, 20);
    assert_eq!(line, "solo");
}

#[test]
fn rolling_year_labels_walk_back_from_today_in_2_month_steps() {
    // Today = 2026-05-23 → 12mo ago = May 2025, 10mo = Jul, …,
    // 2mo = Mar 2026, 0mo = May 2026.
    let today = NaiveDate::from_ymd_opt(2026, 5, 23).unwrap();
    let labels = rolling_year_month_labels(today);
    assert_eq!(
        labels,
        vec!["May", "Jul", "Sep", "Nov", "Jan", "Mar", "May"]
    );
}

#[test]
fn rolling_year_labels_handle_year_boundaries() {
    // Today = 2026-01-15 → 12mo ago = Jan 2025, 10mo = Mar 2025,
    // …, 0mo = Jan 2026. Walks through Mar/May/Jul/Sep/Nov.
    let today = NaiveDate::from_ymd_opt(2026, 1, 15).unwrap();
    let labels = rolling_year_month_labels(today);
    assert_eq!(
        labels,
        vec!["Jan", "Mar", "May", "Jul", "Sep", "Nov", "Jan"]
    );
}
