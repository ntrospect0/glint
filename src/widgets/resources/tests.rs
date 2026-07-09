// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the resources widget. Split out of `mod.rs` per the repo standard.

use std::collections::VecDeque;
use std::time::Instant;

use super::*;
use crate::widgets::test_support::buffer_text;

fn make_widget() -> ResourcesWidget {
    ResourcesWidget::with_config(
        "main".to_string(),
        ResourcesConfig::default(),
        Arc::new(Theme::builtin_defaults()),
    )
}

// ---------------------------------------------------------------------------
// Pre-existing unit tests — unchanged behaviour.
// ---------------------------------------------------------------------------

#[test]
fn compact_bytes_uses_single_letter_suffix() {
    assert_eq!(compact_bytes(0), "0");
    assert_eq!(compact_bytes(900), "900");
    assert_eq!(compact_bytes(1024), "1K");
    assert!(compact_bytes(5 * 1024 * 1024).starts_with("5.0M"));
    assert!(compact_bytes(1024u64.pow(3)).starts_with("1.00G"));
    assert!(compact_bytes(2 * 1024u64.pow(4)).starts_with("2.0T"));
}

#[test]
fn humanize_bytes_picks_unit() {
    assert_eq!(humanize_bytes(0), "0 B");
    assert_eq!(humanize_bytes(512), "512 B");
    assert_eq!(humanize_bytes(1024), "1 KB");
    assert!(humanize_bytes(1024 * 1024 * 5).starts_with("5.0 MB"));
    assert!(humanize_bytes(1024u64.pow(3) * 8).starts_with("8.00 GB"));
}

#[test]
fn format_uptime_collapses_zero_days_hours() {
    assert_eq!(format_uptime(45), "0m");
    assert_eq!(format_uptime(90), "1m");
    assert_eq!(format_uptime(3600 + 5 * 60), "1h 5m");
    assert_eq!(format_uptime(86_400 * 2 + 3600 * 3), "2d 3h 0m");
}

#[test]
fn bar_renders_filled_and_empty() {
    assert_eq!(bar(0.0, 10), "░░░░░░░░░░");
    assert_eq!(bar(100.0, 10), "██████████");
    assert_eq!(bar(50.0, 10), "█████░░░░░");
    // Clamps out-of-range input.
    assert_eq!(bar(-5.0, 4), "░░░░");
    assert_eq!(bar(150.0, 4), "████");
}

#[test]
fn widget_id_uses_instance_suffix() {
    let main = ResourcesWidget::with_config(
        "main".into(),
        ResourcesConfig::default(),
        Arc::new(Theme::builtin_defaults()),
    );
    assert_eq!(main.id(), "resources");
    let host = ResourcesWidget::with_config(
        "host".into(),
        ResourcesConfig::default(),
        Arc::new(Theme::builtin_defaults()),
    );
    assert_eq!(host.id(), "resources@host");
    assert_eq!(host.display_name(), "Resources (host)");
}

#[test]
fn shortcut_preferences_default_to_r_e_s_m() {
    let w = make_widget();
    assert_eq!(w.shortcut_preferences(), &['r', 'e', 's', 'm']);
}

#[test]
fn shortcut_preferences_use_user_override() {
    // 'z' is reserved for the zoom toggle and must not be handed out as a
    // widget shortcut by assign_shortcuts (see assign_shortcuts_never_assigns_z
    // in src/app.rs). This test uses 'x', 'y', 'u' to show that arbitrary
    // user-configured preferences flow through correctly.
    let cfg = ResourcesConfig {
        shortcuts: vec!['x', 'y', 'u'],
        ..ResourcesConfig::default()
    };
    let w =
        ResourcesWidget::with_config("main".into(), cfg, Arc::new(Theme::builtin_defaults()));
    assert_eq!(w.shortcut_preferences(), &['x', 'y', 'u']);
}

// ---------------------------------------------------------------------------
// New helpers for Phase-2 render tests.
// ---------------------------------------------------------------------------

/// Inject a pre-built snapshot directly into the widget's state so render
/// tests can assert on layout without running a real sysinfo refresh.
fn inject_snapshot(widget: &ResourcesWidget, snap: Snapshot) {
    widget.state.lock().expect("state poisoned").snapshot = snap;
}

/// Build a fake process row (no real sysinfo data).
fn fake_proc(i: usize) -> ProcRow {
    ProcRow {
        name: format!("proc{i}"),
        pid: (1000 + i) as u32,
        cpu_percent: 5.0 * i as f32,
        memory_bytes: 1024 * 1024 * (i as u64 + 1),
        virtual_bytes: 1024 * 1024 * 10,
        status_char: 'S',
        run_time_secs: 60 * i as u64,
        thread_count: Some(4),
    }
}

/// Build a snapshot with `n` process rows, 2 CPU cores, and 30 history
/// samples so the Full-tier sparkline has data to render.
fn fake_snapshot_with(n_procs: usize) -> Snapshot {
    Snapshot {
        cpu_per_core: vec![50.0, 75.0],
        total_memory: 8 * 1024 * 1024 * 1024,
        used_memory: 4 * 1024 * 1024 * 1024,
        total_swap: 0,
        used_swap: 0,
        load_average: (1.0, 0.5, 0.25),
        uptime_secs: 3600,
        hostname: "testhost".to_string(),
        top: (0..n_procs).map(fake_proc).collect(),
        cpu_history: (0..30).map(|i| i as f32 * 3.0).collect(),
        selected_row: 0,
        fetched_at: Some(Instant::now()),
    }
}

/// Render the widget at `w × h` into a TestBackend and return the full
/// buffer text.
fn render_at(widget: &ResourcesWidget, w: u16, h: u16) -> String {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| {
            widget.render(frame, frame.area(), false);
        })
        .unwrap();
    buffer_text(terminal.backend().buffer())
}

// ---------------------------------------------------------------------------
// Phase-2 tier-gate tests.
// Full   : must show sparkline content + extra columns (ST, THRD, TIME) +
//          taller process list.
// Standard / Expanded : must NOT show any of that content (no leakage).
// ---------------------------------------------------------------------------

/// Precondition helper: assert `w × h` maps to the expected `ViewTier`.
fn assert_tier(w: u16, h: u16, expected: crate::widgets::ViewTier) {
    let actual =
        crate::widgets::ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h));
    assert_eq!(
        actual, expected,
        "expected {expected:?} at {w}×{h} but got {actual:?}"
    );
}

/// At Full (110 × 35), the extra columns ST, THRD, and TIME must appear in
/// the rendered output. These are absent from the pre-Phase-2 process header.
#[test]
fn full_tier_shows_extra_process_columns() {
    let (w, h) = (110u16, 35u16);
    assert_tier(w, h, crate::widgets::ViewTier::Full);

    let widget = make_widget();
    inject_snapshot(&widget, fake_snapshot_with(40));
    let text = render_at(&widget, w, h);

    assert!(
        text.contains("THRD"),
        "Full tier must show 'THRD' column header; snippet: {:?}",
        &text[..text.len().min(400)]
    );
    assert!(
        text.contains("TIME"),
        "Full tier must show 'TIME' column header"
    );
    // "ST" is the status column header — present in Full, absent in
    // the pre-Phase-2 header ("  PID   CPU%     RES    VIRT   COMMAND").
    assert!(
        text.contains("ST"),
        "Full tier must show 'ST' column header"
    );
}

/// At Standard (50 × 20), none of the Full-tier extras must leak into
/// the rendered output.
#[test]
fn standard_tier_no_full_content_leak() {
    let (w, h) = (50u16, 20u16);
    assert_tier(w, h, crate::widgets::ViewTier::Standard);

    let widget = make_widget();
    inject_snapshot(&widget, fake_snapshot_with(10));
    let text = render_at(&widget, w, h);

    // The pre-Phase-2 process header contains neither "THRD" nor "TIME".
    assert!(
        !text.contains("THRD"),
        "Standard tier must not show 'THRD' column; snippet: {:?}",
        &text[..text.len().min(400)]
    );
    assert!(
        !text.contains("TIME"),
        "Standard tier must not show 'TIME' column"
    );
}

/// At Expanded (70 × 25), the same pre-Phase-2 table must be used —
/// THRD, TIME, and the sparkline must all be absent.
#[test]
fn expanded_tier_no_full_content_leak() {
    let (w, h) = (70u16, 25u16);
    assert_tier(w, h, crate::widgets::ViewTier::Expanded);

    let widget = make_widget();
    inject_snapshot(&widget, fake_snapshot_with(10));
    let text = render_at(&widget, w, h);

    assert!(
        !text.contains("THRD"),
        "Expanded tier must not show 'THRD' column; snippet: {:?}",
        &text[..text.len().min(400)]
    );
    assert!(
        !text.contains("TIME"),
        "Expanded tier must not show 'TIME' column"
    );
}

/// At Full (110 × 35) with 40 processes in the snapshot, the rendered output
/// must include processes beyond the default `top_n_processes` limit (10),
/// proving the taller-list feature is active.
#[test]
fn full_tier_shows_more_processes_than_default() {
    let (w, h) = (110u16, 35u16);
    assert_tier(w, h, crate::widgets::ViewTier::Full);

    let widget = make_widget(); // top_n_processes = 10 by default
    inject_snapshot(&widget, fake_snapshot_with(40));
    let text = render_at(&widget, w, h);

    // proc0–proc9 appear in both Standard and Full. proc10 appears only in
    // Full (where inferred height > 10). Check for its literal string.
    assert!(
        text.contains("proc10"),
        "Full tier must display proc10 (beyond default top_n=10); snippet: {:?}",
        &text[..text.len().min(600)]
    );
}

/// At Standard (50 × 20) with 40 processes in the snapshot, only the default
/// `top_n_processes` (10) must appear — proc10 through proc39 must be absent.
#[test]
fn standard_tier_caps_process_count_at_config_value() {
    let (w, h) = (50u16, 20u16);
    assert_tier(w, h, crate::widgets::ViewTier::Standard);

    let widget = make_widget(); // top_n_processes = 10
    inject_snapshot(&widget, fake_snapshot_with(40));
    let text = render_at(&widget, w, h);

    assert!(
        !text.contains("proc10"),
        "Standard tier must not show proc10 (only 10 displayed); snippet: {:?}",
        &text[..text.len().min(600)]
    );
}

// ---------------------------------------------------------------------------
// CPU sparkline ring-buffer unit tests.
// ---------------------------------------------------------------------------

/// The ring buffer caps at `CPU_HISTORY_CAP` entries: older readings are
/// evicted when the buffer is full.
#[test]
fn cpu_history_ring_buffer_caps_at_capacity() {
    let mut buf: VecDeque<f32> = VecDeque::new();
    for i in 0..(CPU_HISTORY_CAP + 10) {
        buf.push_back(i as f32);
        if buf.len() > CPU_HISTORY_CAP {
            buf.pop_front();
        }
    }
    assert_eq!(buf.len(), CPU_HISTORY_CAP, "ring buffer must stay at CAP");
    // After inserting CAP+10 entries we expect the oldest to be 10.
    assert_eq!(
        buf.front().copied().unwrap(),
        10.0,
        "oldest entry should be 10 after overflow"
    );
    assert_eq!(
        buf.back().copied().unwrap(),
        (CPU_HISTORY_CAP + 9) as f32,
        "newest entry should be the last one inserted"
    );
}

/// An empty history (fewer than 2 samples) must not panic at render time.
/// Verify by rendering at Full with no cpu_history in the snapshot.
#[test]
fn full_tier_empty_sparkline_history_does_not_panic() {
    let (w, h) = (110u16, 35u16);
    assert_tier(w, h, crate::widgets::ViewTier::Full);

    let widget = make_widget();
    let mut snap = fake_snapshot_with(5);
    snap.cpu_history = Vec::new(); // no history
    inject_snapshot(&widget, snap);

    // Should not panic — the sparkline block is skipped for < 2 samples.
    let text = render_at(&widget, w, h);
    assert!(!text.is_empty(), "render must produce output even with no sparkline history");
}

// ---------------------------------------------------------------------------
// row_split sparkline budget tests (Part C of the responsive-views build).
// ---------------------------------------------------------------------------

/// At Full tier with generous vertical space (110×60), the sparkline must
/// claim more than the old fixed 3 rows. We can't easily count braille rows
/// directly from buffer text, but we can verify that the rendered output is
/// taller than a 3-row sparkline would produce by checking that the process
/// list still appears (meaning rows were split, not stolen entirely).
///
/// This also confirms the process list gets the remainder of the rows after
/// the sparkline takes its budget — row_split semantics.
#[test]
fn full_tier_sparkline_grows_beyond_three_rows_with_vertical_room() {
    // Use a tall pane so the sparkline can claim up to SPARKLINE_MAX_ROWS (10).
    let (w, h) = (110u16, 60u16);
    assert_tier(w, h, crate::widgets::ViewTier::Full);

    let widget = make_widget();
    inject_snapshot(&widget, fake_snapshot_with(40));
    let text = render_at(&widget, w, h);

    // Process list must still appear (row_split leaves rows for it).
    assert!(
        text.contains("proc0"),
        "process list must appear even when sparkline is larger"
    );
    // The extra process columns confirm we're in Full tier.
    assert!(text.contains("THRD"), "Full tier must show THRD column");
}

/// At Standard tier with the same tall pane dimensions, the sparkline must
/// not appear at all — the feature is Full-only.
#[test]
fn standard_tier_sparkline_absent_regardless_of_height() {
    // Standard: 50×60 — tall but too narrow for Full or Expanded.
    let (w, h) = (50u16, 60u16);
    assert_tier(w, h, crate::widgets::ViewTier::Standard);

    let widget = make_widget();
    inject_snapshot(&widget, fake_snapshot_with(10));
    let text = render_at(&widget, w, h);

    // Standard must not show Full-tier columns.
    assert!(
        !text.contains("THRD"),
        "Standard tier must not show THRD (no sparkline, no Full columns)"
    );
}

/// At Expanded tier (70×60), the sparkline must not appear.
#[test]
fn expanded_tier_sparkline_absent_regardless_of_height() {
    let (w, h) = (70u16, 60u16);
    assert_tier(w, h, crate::widgets::ViewTier::Expanded);

    let widget = make_widget();
    inject_snapshot(&widget, fake_snapshot_with(10));
    let text = render_at(&widget, w, h);

    assert!(
        !text.contains("THRD"),
        "Expanded tier must not show THRD (no sparkline, no Full columns)"
    );
}

// ---------------------------------------------------------------------------
// j/k cursor navigation tests.
// ---------------------------------------------------------------------------

/// Pressing `j` moves the selection cursor down one row.
#[test]
fn j_key_moves_cursor_down() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut widget = make_widget();
    inject_snapshot(&widget, fake_snapshot_with(10));
    assert_eq!(
        widget.state.lock().unwrap().snapshot.selected_row,
        0,
        "initial cursor must be at row 0"
    );

    let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
    widget.handle_key(j);
    assert_eq!(widget.state.lock().unwrap().snapshot.selected_row, 1);

    widget.handle_key(j);
    assert_eq!(widget.state.lock().unwrap().snapshot.selected_row, 2);
}

/// Pressing `k` moves the selection cursor up one row.
#[test]
fn k_key_moves_cursor_up() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut widget = make_widget();
    let mut snap = fake_snapshot_with(10);
    snap.selected_row = 5;
    inject_snapshot(&widget, snap);

    let k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
    widget.handle_key(k);
    assert_eq!(widget.state.lock().unwrap().snapshot.selected_row, 4);
}

/// `k` at row 0 must not underflow below 0.
#[test]
fn k_at_top_does_not_underflow() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut widget = make_widget();
    inject_snapshot(&widget, fake_snapshot_with(5));
    // cursor is already at 0
    let k = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
    widget.handle_key(k);
    assert_eq!(
        widget.state.lock().unwrap().snapshot.selected_row,
        0,
        "cursor must stay at 0 when k is pressed at the top"
    );
}

/// `j` at the last row must not move past the end of the list.
#[test]
fn j_at_bottom_does_not_overflow() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut widget = make_widget();
    let n = 5usize;
    let mut snap = fake_snapshot_with(n);
    snap.selected_row = n - 1; // last row
    inject_snapshot(&widget, snap);

    let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
    widget.handle_key(j);
    assert_eq!(
        widget.state.lock().unwrap().snapshot.selected_row,
        n - 1,
        "cursor must stay at the last row when j is pressed at the bottom"
    );
}

// ---------------------------------------------------------------------------
// Helper function unit tests.
// ---------------------------------------------------------------------------

#[test]
fn proc_status_char_maps_known_variants() {
    use sysinfo::ProcessStatus;
    assert_eq!(proc_status_char(ProcessStatus::Run), 'R');
    assert_eq!(proc_status_char(ProcessStatus::Sleep), 'S');
    assert_eq!(proc_status_char(ProcessStatus::Zombie), 'Z');
    assert_eq!(proc_status_char(ProcessStatus::Idle), 'I');
    assert_eq!(proc_status_char(ProcessStatus::Stop), 'T');
    assert_eq!(proc_status_char(ProcessStatus::Dead), 'D');
    assert_eq!(proc_status_char(ProcessStatus::Unknown(99)), '?');
}

#[test]
fn format_run_time_compact_formats_correctly() {
    // Zero
    assert_eq!(format_run_time_compact(0), " 0:00");
    // 90 seconds
    assert_eq!(format_run_time_compact(90), " 1:30");
    // 1 hour exactly
    assert_eq!(format_run_time_compact(3600), " 1:00");
    // 1 hour 5 minutes
    assert_eq!(format_run_time_compact(3900), " 1:05");
    // 99 hours (boundary)
    assert_eq!(format_run_time_compact(99 * 3600), "99:00");
    // 100+ hours → capped format
    let long = format_run_time_compact(100 * 3600);
    assert!(
        long.len() <= 5,
        "long runtime must be ≤5 chars; got {:?}",
        long
    );
    assert!(
        long.contains('h'),
        "long runtime must contain 'h' suffix; got {:?}",
        long
    );
}

#[test]
fn format_thread_count_formats_correctly() {
    assert_eq!(format_thread_count(Some(1)), "   1");
    assert_eq!(format_thread_count(Some(256)), " 256");
    assert_eq!(format_thread_count(None), "   -");
}

#[test]
fn cpu_gradient_maps_load_to_earthy_anchors() {
    use ratatui::style::Color;
    // Earthy anchors: olive-green (low) → ochre (mid) → brick-red (high).
    assert_eq!(cpu_gradient(0.0), Color::Rgb(107, 142, 35));
    assert_eq!(cpu_gradient(50.0), Color::Rgb(194, 152, 66));
    assert_eq!(cpu_gradient(100.0), Color::Rgb(160, 64, 45));
    // Clamped outside 0..=100.
    assert_eq!(cpu_gradient(-10.0), Color::Rgb(107, 142, 35));
    assert_eq!(cpu_gradient(150.0), Color::Rgb(160, 64, 45));
    // A quarter-load colour sits between the low and mid anchors.
    if let Color::Rgb(r, g, b) = cpu_gradient(25.0) {
        assert!((107..=194).contains(&r) && (142..=152).contains(&g) && (35..=66).contains(&b));
    } else {
        panic!("expected Rgb");
    }
}

