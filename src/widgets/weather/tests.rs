// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the weather widget. Split out of `mod.rs` per the repo standard.

use super::*;

fn build_widget(cfg: WeatherConfig) -> WeatherWidget {
    WeatherWidget::with_config(
        "main".to_string(),
        cfg,
        Arc::new(Theme::builtin_defaults()),
        ScopedCache::ephemeral(),
    )
}

#[test]
fn render_weather_toml_omits_blank_optionals_and_roundtrips() {
    use crate::wizard::descriptor::WizardValue;
    use std::collections::HashMap;
    // Case 1: all-defaults, blank label + lat/lon → omitted in output.
    let mut values: HashMap<String, WizardValue> = HashMap::new();
    values.insert("label".into(), WizardValue::Text("".into()));
    values.insert("latitude".into(), WizardValue::Text("".into()));
    values.insert("longitude".into(), WizardValue::Text("".into()));
    values.insert("units".into(), WizardValue::Choice("metric".into()));
    values.insert("poll_interval_secs".into(), WizardValue::Number(600.0));
    values.insert("auto_locate".into(), WizardValue::Bool(true));
    let body = render_weather_toml(&values, None);
    assert!(!body.contains("label"));
    assert!(!body.contains("latitude"));
    assert!(!body.contains("longitude"));
    assert!(body.contains("units = \"metric\""));
    let parsed: WeatherConfig = toml::from_str(&body).expect("parses");
    assert!(parsed.label.is_none());
    assert!(parsed.latitude.is_none());
    assert!(parsed.longitude.is_none());
    assert!(parsed.auto_locate);

    // Case 2: explicit coords → keys present, deserialise to Some(_).
    values.insert("label".into(), WizardValue::Text("Richmond, BC".into()));
    values.insert("latitude".into(), WizardValue::Text("49.166".into()));
    values.insert("longitude".into(), WizardValue::Text("-123.133".into()));
    let body = render_weather_toml(&values, None);
    assert!(body.contains("label = \"Richmond, BC\""));
    assert!(body.contains("latitude = 49.166"));
    assert!(body.contains("longitude = -123.133"));
    let parsed: WeatherConfig = toml::from_str(&body).expect("parses");
    assert_eq!(parsed.label.as_deref(), Some("Richmond, BC"));
    assert!((parsed.latitude.unwrap() - 49.166).abs() < 1e-9);
    assert!((parsed.longitude.unwrap() - -123.133).abs() < 1e-9);
}

#[test]
fn default_widget_seeds_richmond_location() {
    let w = WeatherWidget::default();
    let st = w.state.lock().unwrap();
    assert!(st.data.is_none());
    let loc = st
        .location
        .as_ref()
        .expect("default should bake in Richmond");
    assert_eq!(loc.latitude, 49.166);
    assert_eq!(loc.longitude, -123.133);
    assert!(!st.inflight);
    assert!(!st.locating);
}

#[test]
fn explicit_lat_lon_seeds_location_immediately() {
    let cfg = WeatherConfig {
        label: Some("Richmond, BC".into()),
        latitude: Some(49.166),
        longitude: Some(-123.133),
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    let st = w.state.lock().unwrap();
    let loc = st.location.as_ref().expect("location should be seeded");
    assert_eq!(loc.latitude, 49.166);
    assert_eq!(loc.label, "Richmond, BC");
}

#[test]
fn poll_interval_floors_to_thirty_seconds() {
    let cfg = WeatherConfig {
        poll_interval_secs: 5,
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    let interval = w
        .state
        .lock()
        .expect("weather state poisoned")
        .poll
        .interval();
    assert_eq!(interval, Duration::from_secs(30));
}

#[test]
fn format_age_uses_appropriate_units() {
    assert_eq!(format_age(0), "0s");
    assert_eq!(format_age(45), "45s");
    assert_eq!(format_age(59), "59s");
    assert_eq!(format_age(60), "1m");
    assert_eq!(format_age(3599), "59m");
    assert_eq!(format_age(3600), "1h");
    assert_eq!(format_age(86_399), "23h");
    assert_eq!(format_age(86_400), "1d");
    assert_eq!(format_age(86_400 * 5), "5d");
}

#[test]
fn next_action_is_locate_when_no_location_and_auto_locate() {
    // To test the auto-locate path, explicitly clear lat/lon.
    let cfg = WeatherConfig {
        latitude: None,
        longitude: None,
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    assert!(matches!(w.next_action(), NextAction::Locate));
}

#[test]
fn next_action_is_fetch_when_location_known_and_no_recent_attempt() {
    let cfg = WeatherConfig {
        latitude: Some(49.166),
        longitude: Some(-123.133),
        label: Some("Richmond, BC".into()),
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    // Constructor applies a jitter offset so the first fire lands
    // inside the configured window instead of at t=0 (avoids the
    // refresh-storm pile-up with other widgets). Mark dirty here
    // so the test sees the "no recent attempt" branch under test
    // rather than the jitter-deferred state.
    w.state.lock().unwrap().poll.mark_dirty();
    assert!(matches!(w.next_action(), NextAction::Fetch));
}

#[test]
fn next_action_is_wait_when_no_location_and_not_auto_locating() {
    let cfg = WeatherConfig {
        latitude: None,
        longitude: None,
        auto_locate: false,
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    assert!(matches!(w.next_action(), NextAction::Wait));
}
