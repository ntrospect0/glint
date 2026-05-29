// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the resources widget. Split out of `mod.rs` per the repo standard.

use super::*;

fn make_widget() -> ResourcesWidget {
    ResourcesWidget::with_config(
        "main".to_string(),
        ResourcesConfig::default(),
        Arc::new(Theme::builtin_defaults()),
    )
}

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
    let cfg = ResourcesConfig {
        shortcuts: vec!['x', 'y', 'z'],
        ..ResourcesConfig::default()
    };
    let w =
        ResourcesWidget::with_config("main".into(), cfg, Arc::new(Theme::builtin_defaults()));
    assert_eq!(w.shortcut_preferences(), &['x', 'y', 'z']);
}
