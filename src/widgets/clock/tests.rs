// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the clock widget. Split out of `mod.rs` to keep the
//! widget-entry file readable; everything else is unchanged.

use super::clock_view::{city_from_tz_name, day_night_icon};
use super::config::{label_from_iana_zone, render_clock_toml, SecondaryTimezone};
use super::*;
use crate::theme::ColorScheme;
use crate::ui::big_digits;
use chrono::TimeZone;

fn build_widget(cfg: ClockConfig) -> ClockWidget {
    ClockWidget::with_config("main".to_string(), cfg, Arc::new(Theme::builtin_defaults()))
}

#[test]
fn label_from_iana_zone_strips_underscores_and_continent() {
    assert_eq!(label_from_iana_zone("America/New_York"), "New York");
    assert_eq!(label_from_iana_zone("Asia/Tokyo"), "Tokyo");
    assert_eq!(label_from_iana_zone("UTC"), "UTC");
    assert_eq!(label_from_iana_zone("Pacific/Auckland"), "Auckland");
}

#[test]
fn render_clock_toml_emits_secondary_zone_tables() {
    use crate::wizard::descriptor::WizardValue;
    let mut values: std::collections::HashMap<String, WizardValue> = Default::default();
    values.insert(
        "timezone".into(),
        WizardValue::Choice("America/Vancouver".into()),
    );
    values.insert("hour_format".into(), WizardValue::Choice("24h".into()));
    values.insert("show_seconds".into(), WizardValue::Bool(false));
    values.insert("show_date".into(), WizardValue::Bool(true));
    // Two filled secondary-zone slots, one left blank.
    values.insert(
        "secondary_tz_1".into(),
        WizardValue::Choice("America/New_York".into()),
    );
    values.insert(
        "secondary_tz_2".into(),
        WizardValue::Choice("Europe/London".into()),
    );
    values.insert("secondary_tz_3".into(), WizardValue::Choice("".into()));
    let body = render_clock_toml(&values, None);
    assert!(body.contains("timezone = \"America/Vancouver\""));
    assert!(body.contains("hour_format = \"24h\""));
    assert!(body.contains("[[secondary_timezones]]"));
    assert!(body.contains("label = \"New York\""));
    assert!(body.contains("timezone = \"America/New_York\""));
    assert!(body.contains("label = \"London\""));
    // Round-trips through the existing deserialiser; the empty slot is
    // omitted entirely.
    let parsed: ClockConfig = toml::from_str(&body).expect("wizard-rendered clock.toml parses");
    assert_eq!(parsed.timezone.as_deref(), Some("America/Vancouver"));
    assert_eq!(parsed.hour_format, 24);
    assert_eq!(parsed.secondary_timezones.len(), 2);
}

#[test]
fn twelve_hour_format_renders_midnight_as_12_am() {
    let cfg = ClockConfig {
        timezone: Some("UTC".into()),
        show_seconds: false,
        show_seconds_ticker: false,
        show_date: false,
        hour_format: 12,
        secondary_timezones: Vec::new(),
        gradient: big_digits::Gradient::default(),
        colors: ColorScheme::default(),
        shortcuts: Vec::new(),
    };
    let widget = build_widget(cfg);
    let midnight_utc = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
    let (time, ampm, date) = widget.render_strings(midnight_utc);
    assert_eq!(time, "12:00");
    assert_eq!(ampm, "AM");
    assert!(date.is_empty());
}

#[test]
fn twenty_four_hour_format_zero_pads() {
    let cfg = ClockConfig {
        timezone: Some("UTC".into()),
        show_seconds: true,
        show_seconds_ticker: false,
        show_date: false,
        hour_format: 24,
        secondary_timezones: Vec::new(),
        gradient: big_digits::Gradient::default(),
        colors: ColorScheme::default(),
        shortcuts: Vec::new(),
    };
    let widget = build_widget(cfg);
    let t = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 9, 5, 7).unwrap();
    let (time, ampm, _) = widget.render_strings(t);
    assert_eq!(time, "09:05:07");
    assert_eq!(ampm, "");
}

#[test]
fn ticker_includes_seconds_in_primary_timezone() {
    let cfg = ClockConfig {
        timezone: Some("UTC".into()),
        show_seconds: false,
        show_seconds_ticker: true,
        show_date: false,
        hour_format: 24,
        secondary_timezones: Vec::new(),
        gradient: big_digits::Gradient::default(),
        colors: ColorScheme::default(),
        shortcuts: Vec::new(),
    };
    let w = build_widget(cfg);
    let t = chrono::Utc.with_ymd_and_hms(2026, 1, 2, 9, 5, 42).unwrap();
    assert_eq!(w.ticker_string(t), "09:05:42");
}

#[test]
fn city_from_tz_name_strips_region_and_underscores() {
    assert_eq!(city_from_tz_name("America/New_York"), "New York");
    assert_eq!(city_from_tz_name("Europe/London"), "London");
    assert_eq!(city_from_tz_name("Asia/Tokyo"), "Tokyo");
    assert_eq!(city_from_tz_name("UTC"), "UTC");
}

#[test]
fn world_clock_entries_pin_local_during_time_override() {
    use chrono_tz::Tz;
    let cfg = ClockConfig {
        secondary_timezones: vec![SecondaryTimezone {
            label: "Tokyo".into(),
            timezone: "Asia/Tokyo".into(),
        }],
        ..ClockConfig::default()
    };
    let w = build_widget(cfg);
    {
        let mut st = w.state.lock().unwrap();
        st.transient_tz = Some(("Berlin".into(), "Europe/Berlin".parse::<Tz>().unwrap()));
    }
    let entries = w.world_clock_entries();
    assert_eq!(entries.len(), 3, "Local + override + 1 secondary");
    assert_eq!(entries[0].0, "Local");
    assert_eq!(entries[1].0, "Berlin");
    assert_eq!(entries[2].0, "Tokyo");
}

#[test]
fn world_clock_entries_lead_with_primary() {
    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: vec![SecondaryTimezone {
            label: "Tokyo".into(),
            timezone: "Asia/Tokyo".into(),
        }],
        ..ClockConfig::default()
    };
    let w = build_widget(cfg);
    let entries = w.world_clock_entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "Vancouver");
    assert_eq!(entries[1].0, "Tokyo");
}

#[test]
fn world_clock_entries_include_icon_time_and_date() {
    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: vec![SecondaryTimezone {
            label: "Tokyo".into(),
            timezone: "Asia/Tokyo".into(),
        }],
        ..ClockConfig::default()
    };
    let w = build_widget(cfg);
    let entries = w.world_clock_entries();
    for (_label, formatted) in &entries {
        // Format: "<icon> HH:MM Wkd Mon DD"
        let parts: Vec<&str> = formatted.split_whitespace().collect();
        assert_eq!(parts.len(), 5, "unexpected format: {formatted:?}");
        assert!(parts[0] == "☀" || parts[0] == "☾");
        // HH:MM
        assert_eq!(parts[1].chars().nth(2), Some(':'));
        // Weekday abbreviation
        assert!(
            ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].contains(&parts[2]),
            "unexpected weekday: {:?}",
            parts[2]
        );
        // Month abbreviation
        assert!(
            [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                "Dec"
            ]
            .contains(&parts[3]),
            "unexpected month: {:?}",
            parts[3]
        );
        // Day-of-month is a positive integer
        assert!(parts[4].parse::<u32>().is_ok());
    }
}

#[test]
fn day_night_icon_boundaries() {
    assert_eq!(day_night_icon(5), "☾");
    assert_eq!(day_night_icon(6), "☀");
    assert_eq!(day_night_icon(12), "☀");
    assert_eq!(day_night_icon(17), "☀");
    assert_eq!(day_night_icon(18), "☾");
    assert_eq!(day_night_icon(23), "☾");
    assert_eq!(day_night_icon(0), "☾");
}

#[test]
fn scroll_world_clocks_clamps_and_passes_through_when_full_list_fits() {
    let w = build_widget(ClockConfig::default());

    // max_scroll == 0 means the whole list fits, so ↑/↓ events should
    // fall through (Ignored) rather than silently swallow the keypress.
    assert_eq!(w.scroll_world_clocks(-1), EventResult::Ignored);
    assert_eq!(w.scroll_world_clocks(1), EventResult::Ignored);
    assert_eq!(w.state.lock().unwrap().world_clock_scroll, 0);

    // Simulate a render that left 3 entries hidden below the fold.
    {
        let mut st = w.state.lock().unwrap();
        st.world_clock_max_scroll = 3;
    }
    // Scroll down advances; can't go past max_scroll.
    assert_eq!(w.scroll_world_clocks(1), EventResult::Handled);
    assert_eq!(w.state.lock().unwrap().world_clock_scroll, 1);
    for _ in 0..10 {
        w.scroll_world_clocks(1);
    }
    assert_eq!(
        w.state.lock().unwrap().world_clock_scroll,
        3,
        "scroll must clamp at max_scroll"
    );
    // Scroll up walks back; can't go below 0.
    assert_eq!(w.scroll_world_clocks(-1), EventResult::Handled);
    assert_eq!(w.state.lock().unwrap().world_clock_scroll, 2);
    for _ in 0..10 {
        w.scroll_world_clocks(-1);
    }
    assert_eq!(
        w.state.lock().unwrap().world_clock_scroll,
        0,
        "scroll must clamp at 0"
    );
}

#[test]
fn invalid_secondary_timezones_are_dropped() {
    let cfg = ClockConfig {
        secondary_timezones: vec![
            SecondaryTimezone {
                label: "New York".into(),
                timezone: "America/New_York".into(),
            },
            SecondaryTimezone {
                label: "Bogus".into(),
                timezone: "Not/A_Real_TZ".into(),
            },
        ],
        ..ClockConfig::default()
    };
    let w = build_widget(cfg);
    assert_eq!(w.secondaries.len(), 1);
    assert_eq!(w.secondaries[0].0, "New York");
}
