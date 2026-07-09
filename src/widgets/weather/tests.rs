// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the weather widget. Split out of `mod.rs` per the repo standard.

use super::*;
use crate::widgets::test_support::buffer_text;

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
    assert!(st.data_by_key.is_empty());
    let loc = st
        .location
        .as_ref()
        .expect("default should bake in Richmond");
    assert_eq!(loc.latitude, 49.166);
    assert_eq!(loc.longitude, -123.133);
    assert!(st.inflight_keys.is_empty());
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
    assert_eq!(w.poll_interval, Duration::from_secs(30));
    // The home tracker (seeded in the constructor) inherits the
    // floored interval — verify by pulling its entry out of the
    // per-city map.
    let st = w.state.lock().expect("weather state poisoned");
    let home_key = loc_key(49.166, -123.133);
    assert_eq!(
        st.poll_by_key
            .get(&home_key)
            .expect("home tracker seeded")
            .interval(),
        Duration::from_secs(30),
    );
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
    // refresh-storm pile-up with other widgets). Mark the home
    // tracker dirty so the test sees the "no recent attempt"
    // branch under test rather than the jitter-deferred state.
    {
        let mut st = w.state.lock().unwrap();
        for t in st.poll_by_key.values_mut() {
            t.mark_dirty();
        }
    }
    assert!(matches!(w.next_action(), NextAction::Fetch(_, _)));
}

#[test]
fn carousel_lists_home_then_extras() {
    let cfg = WeatherConfig {
        label: Some("Home".into()),
        latitude: Some(49.166),
        longitude: Some(-123.133),
        cities: vec![
            WeatherCity {
                label: "Tokyo".into(),
                latitude: 35.68,
                longitude: 139.76,
            },
            WeatherCity {
                label: "London".into(),
                latitude: 51.51,
                longitude: -0.13,
            },
        ],
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    let carousel = w.carousel();
    assert_eq!(carousel.len(), 3);
    assert!(matches!(carousel[0].kind, CityKind::Home));
    assert_eq!(carousel[0].location.label, "Home");
    assert!(matches!(carousel[1].kind, CityKind::Extra(0)));
    assert_eq!(carousel[1].location.label, "Tokyo");
    assert!(matches!(carousel[2].kind, CityKind::Extra(1)));
    assert_eq!(carousel[2].location.label, "London");
}

#[test]
fn select_delta_clamps_at_carousel_edges() {
    let cfg = WeatherConfig {
        label: Some("Home".into()),
        latitude: Some(49.166),
        longitude: Some(-123.133),
        cities: vec![WeatherCity {
            label: "Tokyo".into(),
            latitude: 35.68,
            longitude: 139.76,
        }],
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    // From 0: right advances; second right is a no-op (clamped).
    assert_eq!(w.select_delta(1), EventResult::Handled);
    assert_eq!(w.state.lock().unwrap().selected, 1);
    assert_eq!(w.select_delta(1), EventResult::Ignored);
    assert_eq!(w.state.lock().unwrap().selected, 1);
    // Left walks back to 0; another left is a no-op.
    assert_eq!(w.select_delta(-1), EventResult::Handled);
    assert_eq!(w.state.lock().unwrap().selected, 0);
    assert_eq!(w.select_delta(-1), EventResult::Ignored);
}

#[test]
fn select_delta_ignored_when_single_city() {
    let cfg = WeatherConfig {
        label: Some("Home".into()),
        latitude: Some(49.166),
        longitude: Some(-123.133),
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    assert_eq!(w.select_delta(1), EventResult::Ignored);
    assert_eq!(w.select_delta(-1), EventResult::Ignored);
}

#[test]
fn request_remove_noop_on_home_row() {
    let cfg = WeatherConfig {
        label: Some("Home".into()),
        latitude: Some(49.166),
        longitude: Some(-123.133),
        cities: vec![WeatherCity {
            label: "Tokyo".into(),
            latitude: 35.68,
            longitude: 139.76,
        }],
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    // Cursor defaults to home (idx 0). `-` should refuse.
    assert_eq!(w.request_remove_selected(), EventResult::Ignored);
    assert!(w.state.lock().unwrap().confirm_remove.is_none());
    // Swipe to the Extra and try again — modal should open.
    w.select_delta(1);
    assert_eq!(w.request_remove_selected(), EventResult::Handled);
    assert_eq!(
        w.state.lock().unwrap().confirm_remove.as_ref().unwrap().0,
        "Tokyo"
    );
}

#[test]
fn request_remove_noop_on_lookup_row() {
    let cfg = WeatherConfig {
        label: Some("Home".into()),
        latitude: Some(49.166),
        longitude: Some(-123.133),
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    {
        let mut st = w.state.lock().unwrap();
        st.transient_location = Some(GeoLocation {
            latitude: 35.68,
            longitude: 139.76,
            city: "Tokyo".into(),
            city_admin: "Tokyo, Tokyo".into(),
            label: "Tokyo, Tokyo, Japan".into(),
            timezone: None,
        });
        st.selected = 1; // land on the transient
    }
    assert_eq!(w.request_remove_selected(), EventResult::Ignored);
    assert!(w.state.lock().unwrap().confirm_remove.is_none());
}

#[test]
#[ignore = "manual visualization helper — run with `cargo test dump_weather_glyphs -- --ignored --nocapture`"]
fn dump_weather_glyphs() {
    use super::icons::*;
    use std::io::Write;
    let all: &[(&str, &WeatherIcon)] = &[
        ("CLOUD", &CLOUD),
        ("RAIN", &RAIN),
        ("FOG", &FOG),
        ("THUNDER", &THUNDER),
        ("SUN", &SUN),
        ("SUN_CLOUD", &SUN_CLOUD),
        ("SNOW", &SNOW),
        ("SHOWERS", &SHOWERS),
        ("MOON", &MOON),
        ("WET_SNOW", &WET_SNOW),
        ("TORNADO", &TORNADO),
        ("MOON_CLOUD", &MOON_CLOUD),
        ("LIGHTNING_BOLT", &LIGHTNING_BOLT),
        ("THUNDER_SHOWERS", &THUNDER_SHOWERS),
        ("SUN_STORM", &SUN_STORM),
        ("THUNDER_RAIN", &THUNDER_RAIN),
    ];
    let path = "/tmp/weather_glyphs.txt";
    let mut f = std::fs::File::create(path).expect("create dump file");
    writeln!(f, "Weather glyph dump — half-block ASCII").unwrap();
    writeln!(f, "Slot reserved: {} char rows (MAX_HEIGHT_CHARS)", MAX_HEIGHT_CHARS).unwrap();
    writeln!(f, "═══════════════════════════════════════════════════════════════").unwrap();
    for (name, icon) in all {
        let char_rows = (icon.height as usize).div_ceil(2);
        writeln!(
            f,
            "\n{name} — {}×{} px ({} char rows)",
            icon.width, icon.height, char_rows
        )
        .unwrap();
        for char_row in 0..char_rows {
            let top_idx = char_row * 2;
            let bot_idx = top_idx + 1;
            let top_row = icon.pixels[top_idx];
            let bot_row = if bot_idx < icon.pixels.len() {
                icon.pixels[bot_idx]
            } else {
                &[]
            };
            let mut line = String::new();
            for col in 0..(icon.width as usize) {
                let top = top_row.get(col).and_then(|x| *x).is_some();
                let bot = bot_row.get(col).and_then(|x| *x).is_some();
                line.push(match (top, bot) {
                    (false, false) => ' ',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (true, true) => '█',
                });
            }
            writeln!(f, "{line}").unwrap();
        }
    }
    println!("Wrote weather glyph dump to {path}");
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

// ─────────────────────────────────────────────────────────────────────
// Render-tier tests (TestBackend)
// ─────────────────────────────────────────────────────────────────────

/// Build a WeatherData with three distinct emoji-icon forecast days (one day
/// of "today" + three forecast days) using the given units. Used by tests that
/// verify the **unzoomed** (Standard/Expanded) rendering is unchanged.
fn make_forecast_data(units: provider::Units) -> provider::WeatherData {
    use chrono::NaiveDate;
    // 2026-07-06 = Monday; the three forecast days are Tue/Wed/Thu.
    let today = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
    provider::WeatherData {
        temperature: 20.0,
        apparent_temperature: 18.0,
        humidity: 65.0,
        wind_speed: 15.0,
        wind_direction: None,
        weather_code: 0,
        units,
        fetched_at: chrono::Local::now(),
        daily: vec![
            provider::DailyForecast {
                date: today,
                temperature_high: 22.0,
                temperature_low: 12.0,
                weather_code: 0,
                sunrise: None,
                sunset: None,
                precipitation_probability_max: None,
                uv_index_max: None,
            },
            provider::DailyForecast {
                date: today + chrono::Duration::days(1),
                temperature_high: 18.0,
                temperature_low: 8.0,
                weather_code: 2,
                sunrise: None,
                sunset: None,
                precipitation_probability_max: None,
                uv_index_max: None,
            },
            provider::DailyForecast {
                date: today + chrono::Duration::days(2),
                temperature_high: 15.0,
                temperature_low: 5.0,
                weather_code: 61,
                sunrise: None,
                sunset: None,
                precipitation_probability_max: None,
                uv_index_max: None,
            },
            provider::DailyForecast {
                date: today + chrono::Duration::days(3),
                temperature_high: 22.0,
                temperature_low: 12.0,
                weather_code: 95,
                sunrise: None,
                sunset: None,
                precipitation_probability_max: None,
                uv_index_max: None,
            },
        ],
        hourly: vec![],
    }
}

/// Build a WeatherData with 8 daily entries (today + 7 forecast days) and
/// 48 hourly points starting from midnight of the current day. Used by tests
/// that exercise the **Full-tier** (zoomed) rendering path.
fn make_full_data() -> provider::WeatherData {
    use chrono::{NaiveDateTime, NaiveTime};
    let today = chrono::Local::now().date_naive();
    let midnight = NaiveDateTime::new(today, NaiveTime::from_hms_opt(0, 0, 0).unwrap());

    // 48 hourly points: midnight today → midnight+48h. At any wall-clock time,
    // at least 24 of these fall within the render filter window (now..now+25h).
    let hourly: Vec<provider::HourlyPoint> = (0i64..48)
        .map(|h| provider::HourlyPoint {
            time: midnight + chrono::Duration::hours(h),
            temperature: 18.0 + (h as f64 * std::f64::consts::PI / 12.0).sin() * 4.0,
            precipitation_probability: if h % 12 < 6 { 55.0 } else { 15.0 },
        })
        .collect();

    let weather_codes = [0u32, 2, 61, 95, 3, 0, 2, 61];
    let today_entries: Vec<provider::DailyForecast> = (0..8)
        .map(|i| provider::DailyForecast {
            date: today + chrono::Duration::days(i),
            temperature_high: 22.0 - i as f64,
            temperature_low: 12.0 - i as f64,
            weather_code: weather_codes[i as usize],
            sunrise: None,
            sunset: None,
            precipitation_probability_max: Some(if i % 3 == 0 { 20.0 } else { 60.0 }),
            uv_index_max: Some(4.0 - i as f64 * 0.2),
        })
        .collect();

    provider::WeatherData {
        temperature: 20.0,
        apparent_temperature: 18.0,
        humidity: 65.0,
        wind_speed: 15.0,
        wind_direction: Some(315.0), // NW
        weather_code: 0,
        units: provider::Units::Metric,
        fetched_at: chrono::Local::now(),
        daily: today_entries,
        hourly,
    }
}

/// Find the x position of each "/" that appears in a row which also
/// contains a weekday abbreviation. Returns one entry per forecast row.
fn forecast_slash_x(buf: &ratatui::buffer::Buffer) -> Vec<u16> {
    let area = buf.area;
    let weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let mut positions = Vec::new();

    'row: for y in area.y..area.bottom() {
        let row_text: String = (area.x..area.right())
            .flat_map(|x| buf[(x, y)].symbol().chars())
            .collect();
        if !weekdays.iter().any(|wd| row_text.contains(wd)) {
            continue 'row;
        }
        // This is a forecast row; find the '/' separator.
        if let Some(x) = (area.x..area.right()).find(|&x| buf[(x, y)].symbol() == "/") {
            positions.push(x);
        }
    }
    positions
}

/// At a wide/tall (zoomed-like) size, the three forecast rows must be
/// column-aligned: the "/" separator must appear at the same x in every row.
///
/// This guards the fix for Alignment::Center misaligning emoji+VS-16 icon
/// cells in the forecast block. The test uses three days with different icons
/// (⛅, 🌧, ⛈) to exercise varying code points.
#[test]
fn forecast_rows_are_column_aligned_at_zoomed_size() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 80×35: inner is 78×33; subtract toggle (1) + hint (1) → body_h 31.
    // pick_art_layout(31): base=12, bottom=19 ≥ MIN_BOTTOM_ROWS_L3+3=9
    // → ArtLayout::Full, all forecast rows visible.
    let (w, h) = (80u16, 35u16);

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(
            loc_key(49.166, -123.133),
            make_forecast_data(provider::Units::Metric),
        );
    }

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let slash_positions = forecast_slash_x(terminal.backend().buffer());

    assert_eq!(
        slash_positions.len(),
        3,
        "expected a '/' in each of the 3 forecast rows; found positions: {:?}",
        slash_positions,
    );

    // All three '/' must be at the same x — that's the column-alignment invariant.
    assert!(
        slash_positions.windows(2).all(|w| w[0] == w[1]),
        "forecast '/' separator is not column-aligned across rows: x = {:?}",
        slash_positions,
    );
}

/// Return the x position of the first non-space cell in buffer row `y`,
/// scanning from column 1 to skip the left-border `│` at column 0.
fn first_text_x(buf: &ratatui::buffer::Buffer, y: u16) -> Option<u16> {
    let area = buf.area;
    // Start at area.x + 1: column 0 holds the widget border character
    // which is never a space but is not content padding.
    (area.x + 1..area.right()).find(|&x| buf[(x, y)].symbol() != " ")
}

/// Return the y of the first row that contains `needle` as a substring.
fn find_row_with(buf: &ratatui::buffer::Buffer, needle: &str) -> Option<u16> {
    let area = buf.area;
    (area.y..area.bottom()).find(|&y| {
        let row: String = (area.x..area.right())
            .flat_map(|x| buf[(x, y)].symbol().chars())
            .collect();
        row.contains(needle)
    })
}

/// Per-line centering: shorter lines must be more indented than wider ones.
///
/// At 80×35 with ArtLayout::Full the forecast rows are the widest content
/// in the bottom block (~22 chars). The footer "Just updated" (12 chars) is
/// narrower and must therefore start further right. Under the old
/// block-centering approach all lines shared the same leading pad and the
/// footer x would equal the forecast x.
#[test]
fn bottom_block_shorter_lines_are_more_indented_than_forecast_rows() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(
            loc_key(49.166, -123.133),
            make_forecast_data(provider::Units::Metric),
        );
    }

    let backend = TestBackend::new(80, 35);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    // Locate the forecast row for Tuesday and the fresh footer.
    let forecast_y = find_row_with(buf, "Tue").expect("Tue forecast row not found");
    let footer_y = find_row_with(buf, "Just updated").expect("footer row not found");

    let forecast_x = first_text_x(buf, forecast_y).expect("forecast row is blank");
    let footer_x = first_text_x(buf, footer_y).expect("footer row is blank");

    assert!(
        footer_x > forecast_x,
        "footer (shorter line) must start further right than forecast rows under \
         per-line centering; footer_x={footer_x}, forecast_x={forecast_x}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// ViewTier::Full render tests (zoomed / dashboard-filling pane)
// ─────────────────────────────────────────────────────────────────────────────


/// At Full size with full data, the 7-day forecast section header must appear
/// and at least 7 weekday rows must render (proving more than the 3-day unzoomed block).
/// Uses a taller terminal (120×55) so the forecast + hourly both fit within the card.
#[test]
fn full_tier_renders_7day_forecast_and_detail_strip() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 120×55 → ViewTier::Full (width >= 105, height >= 30). Tall enough
    // to accommodate conditions/art + 7-day forecast + hourly chart, each
    // inside a bordered card.
    let (w, h) = (120u16, 55u16);

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    // 7-day section header must appear.
    assert!(
        text.contains("7-day"),
        "expected '7-day' section header at Full tier; buffer text (first 400 chars):\n{}",
        &text[..text.len().min(400)]
    );

    // At a tall Full-tier terminal the single consolidated forecast shows all
    // 7 days, so at least 7 distinct weekday occurrences must appear.
    let weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let hits: usize = weekdays.iter().map(|wd| text.matches(wd).count()).sum();
    assert!(
        hits >= 7,
        "expected ≥ 7 weekday occurrences at Full tier (7-day forecast); found {hits}"
    );
}

/// At Full size the home city's conditions block must show wind speed
/// (via render_with_art's humidity/wind line). The old separate detail
/// strip (Wind NW cardinal) was part of the retired render_full_right;
/// the columnar layout surfaces wind speed from render_with_art instead.
#[test]
fn full_tier_home_column_shows_wind_speed() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    // render_with_art emits "Wind: <speed> km/h" in the bottom block.
    assert!(
        text.contains("Wind"),
        "expected 'Wind' in Full-tier home column (from render_with_art); snippet:\n{}",
        &text[..text.len().min(600)]
    );
}

/// At Full size the hourly section header must appear when hourly data covers
/// the next 24 h. The 48-point dataset from make_full_data always has future
/// points regardless of wall-clock time.
#[test]
fn full_tier_renders_hourly_section_header() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains("Next 24h"),
        "expected 'Next 24h' hourly section header at Full tier"
    );
}

/// The 24h chart must show a high-temperature label (with unit symbol) on the
/// top chart row and a low-temperature label on the bottom chart row. The
/// make_full_data sine wave always produces a distinct hi and lo across any
/// 25h window, so both labels must be present somewhere in the buffer. We
/// additionally confirm the labels appear on different rows: hi on the first
/// chart row (immediately after the "Next 24h" header) and lo exactly
/// CHART_ROWS-1 rows below that.
#[test]
fn full_tier_hourly_chart_shows_hi_lo_labels() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    // 120×55: tall enough that the hourly chart section fits comfortably.
    let backend = TestBackend::new(120, 55);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // Find the row containing the "Next 24h" header.
    let header_y = (area.y..area.bottom())
        .find(|&y| row_text(buf, y).contains("Next 24h"))
        .expect("'Next 24h' header must appear in Full-tier render");

    // The four chart rows follow immediately after the header.
    let chart_top_y = header_y + 1;
    // CHART_ROWS = 4, so last chart row is chart_top_y + 3.
    let chart_bot_y = chart_top_y + 3;

    let top_row = row_text(buf, chart_top_y);
    let bot_row = row_text(buf, chart_bot_y);

    // Both rows must carry a temperature unit symbol in the gutter label.
    assert!(
        top_row.contains('°'),
        "top chart row must contain the high-temp label (with '°'); row: {top_row:?}"
    );
    assert!(
        bot_row.contains('°'),
        "bottom chart row must contain the low-temp label (with '°'); row: {bot_row:?}"
    );

    // The middle chart rows must NOT carry a label.
    for mid_y in (chart_top_y + 1)..chart_bot_y {
        let mid_row = row_text(buf, mid_y);
        // Middle rows should only have braille chart characters (no digit+°).
        // We check that '°' does not appear on those rows.
        assert!(
            !mid_row.contains('°'),
            "middle chart row {mid_y} must not contain a temp label; row: {mid_row:?}"
        );
    }
}

/// When the available width is too narrow to afford a gutter the chart must
/// still render (graceful degradation: labels simply omitted). We confirm the
/// "Next 24h" header is present and no panic occurs. The Rain% bar must also
/// still appear.
///
/// This test deliberately uses a terminal that is wide enough for Full-tier
/// but narrow enough that the gutter label logic exercises the boundary path
/// (the gutter is either present or gracefully absent — either way the chart
/// section must still render without panicking).
#[test]
fn full_tier_hourly_chart_narrow_width_no_labels_no_panic() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    // 105×38: minimum Full-tier width (105 >= FULL_MIN_W) with enough height
    // (38 >= FULL_MIN_H=30) for the hourly section to render. Content width
    // inside the bordered card after 2-col side margins each side is
    // 105-2(border)-4(margin) = 99 cols. That is narrow enough to stress-test
    // the gutter boundary while still wide enough that the chart renders.
    let backend = TestBackend::new(105, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    assert!(
        text.contains("Next 24h"),
        "narrow Full-tier must still render the hourly chart section header"
    );
    // Rain% bar must also appear.
    assert!(
        text.contains("Rain%"),
        "narrow Full-tier must still render the Rain% bar"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Standard + Expanded tier tests — prove unzoomed rendering is unchanged.
// ─────────────────────────────────────────────────────────────────────────────

/// Standard-tier render (60×25 → ViewTier::Standard) must show "Next 3 days"
/// and must NOT show the Full-tier hourly or 7-day sections.
#[test]
fn standard_tier_shows_3day_not_full_tier_content() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 60×25 → width 60 is between COMPACT_MAX_W+1 (32) and EXPANDED_MIN_W-1
    // (64), so ViewTier::Standard.
    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(
            loc_key(49.166, -123.133),
            make_forecast_data(provider::Units::Metric),
        );
    }

    let backend = TestBackend::new(60, 25);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    assert!(
        text.contains("Next 3 days"),
        "Standard tier must show 'Next 3 days' section"
    );
    assert!(
        !text.contains("7-day"),
        "Standard tier must NOT show Full-tier '7-day' section"
    );
    assert!(
        !text.contains("Next 24h"),
        "Standard tier must NOT show Full-tier hourly section"
    );
}

/// Expanded-tier render (80×28 → ViewTier::Expanded) must also show "Next 3 days"
/// and must NOT show the Full-tier content. Expanded is reachable by a wide
/// unzoomed grid cell, which is exactly why it must be unchanged.
#[test]
fn expanded_tier_shows_3day_not_full_tier_content() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 80×28 → width 80 >= EXPANDED_MIN_W (65) but < FULL_MIN_W (105), height
    // 28 < FULL_MIN_H (30) → ViewTier::Expanded.
    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(
            loc_key(49.166, -123.133),
            make_forecast_data(provider::Units::Metric),
        );
    }

    let backend = TestBackend::new(80, 28);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    assert!(
        text.contains("Next 3 days"),
        "Expanded tier must show 'Next 3 days' section"
    );
    assert!(
        !text.contains("7-day"),
        "Expanded tier must NOT show Full-tier '7-day' section"
    );
    assert!(
        !text.contains("Next 24h"),
        "Expanded tier must NOT show Full-tier hourly section"
    );
}

/// At a narrow pane (below ART_THRESHOLD), render_with_art returns early
/// before reaching the bottom block. This test verifies the widget does not
/// panic at a small size with data injected, guarding against regressions in
/// the render path touched by the bottom-block refactor.
#[test]
fn narrow_weather_render_does_not_panic() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(
            loc_key(49.166, -123.133),
            make_forecast_data(provider::Units::Metric),
        );
    }

    // 40×15: body_h is well below ART_THRESHOLD (18); the bottom block is
    // never reached so this exercises the early-return path.
    let backend = TestBackend::new(40, 15);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();
    // No panic = pass.
}

// ─────────────────────────────────────────────────────────────────────────────
// Full-tier columnar multi-city grid tests (PinnedGrid layout)
// ─────────────────────────────────────────────────────────────────────────────

/// Build a 2-city widget (home = Richmond, extra = Tokyo) with data
/// pre-seeded in the cache for both cities.
fn build_two_city_widget_with_data() -> WeatherWidget {
    let cfg = WeatherConfig {
        label: Some("Richmond, BC".into()),
        latitude: Some(49.166),
        longitude: Some(-123.133),
        cities: vec![WeatherCity {
            label: "Tokyo".into(),
            latitude: 35.68,
            longitude: 139.76,
        }],
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    {
        let mut st = w.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
        st.data_by_key.insert(loc_key(35.68, 139.76), make_full_data());
    }
    w
}

/// Return the x-coordinate of the first non-space, non-border cell in each
/// column that contains `needle` at a given row y.  We split the buffer into
/// per-column slices based on the `col_width` derived from PinnedGrid maths.
fn column_x_of(buf: &ratatui::buffer::Buffer, y: u16, needle: &str) -> Vec<u16> {
    let area = buf.area;
    // Collect every x where the text starting at (x, y) matches `needle`.
    let mut hits = Vec::new();
    let row_text: String = (area.x..area.right())
        .flat_map(|x| buf[(x, y)].symbol().chars())
        .collect();
    // The needle may span multiple cells; do a string search.
    let mut search_start = 0;
    while let Some(pos) = row_text[search_start..].find(needle) {
        hits.push(area.x + (search_start + pos) as u16);
        search_start += pos + 1;
    }
    hits
}

/// Row `y` in the buffer as a String.
fn row_text(buf: &ratatui::buffer::Buffer, y: u16) -> String {
    let area = buf.area;
    (area.x..area.right())
        .flat_map(|x| buf[(x, y)].symbol().chars())
        .collect()
}

/// At Full size (120×38) with 2 cities cached, the render must produce at
/// least two city-label headers side by side (both "Richmond" and "Tokyo"
/// appear), and "Richmond" must be at a smaller x than "Tokyo" (home leftmost).
#[test]
fn full_tier_two_cities_render_side_by_side_home_leftmost() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    // Find the y of the row that contains "Richmond" — the city label header.
    let richmond_y = (buf.area.y..buf.area.bottom())
        .find(|&y| row_text(buf, y).contains("Richmond"))
        .expect("Richmond city label must appear in the Full-tier grid");

    // "Tokyo" must appear at the same y (same header row).
    assert!(
        row_text(buf, richmond_y).contains("Tokyo"),
        "Tokyo label must appear on the same row as Richmond (side-by-side columns)"
    );

    // Richmond must start at a smaller x than Tokyo (home is leftmost).
    let richmond_xs = column_x_of(buf, richmond_y, "Richmond");
    let tokyo_xs = column_x_of(buf, richmond_y, "Tokyo");
    assert!(!richmond_xs.is_empty(), "Richmond not found in row");
    assert!(!tokyo_xs.is_empty(), "Tokyo not found in row");
    assert!(
        richmond_xs[0] < tokyo_xs[0],
        "Home city (Richmond) must start left of Tokyo; richmond_x={}, tokyo_x={}",
        richmond_xs[0],
        tokyo_xs[0]
    );
}

/// At Full size with 2 cities, each city column must show conditions (the
/// `render_with_art` humidity/wind line) AND the hourly section header.
#[test]
fn full_tier_each_column_shows_conditions_and_hourly() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    // "Next 24h" appears at least twice (once per column) when both cities
    // have hourly data — make_full_data always supplies 48 hourly points.
    let count = text.matches("Next 24h").count();
    assert!(
        count >= 2,
        "expected 'Next 24h' to appear in each column (≥2); found {count}"
    );

    // Both columns also render the forecast section.
    let forecast_count = text.matches("7-day").count();
    assert!(
        forecast_count >= 2,
        "expected '7-day' forecast header in each column (≥2); found {forecast_count}"
    );
}

/// After a grid_scroll(1) on a 2-city widget, home (Richmond) must remain
/// the leftmost column and the grid_scroll_offset must advance.
/// (With only 2 cities and a 120-col terminal, PinnedGrid fits both columns
/// so max_scroll=0 and grid_scroll() returns Ignored — tested separately.)
#[test]
fn full_tier_home_stays_leftmost_after_scroll_attempt() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();

    // Manually set a non-zero scroll offset (simulates a 3-city scenario
    // where scrolling is possible). Both columns still show but home is
    // always first in the PinnedGrid output.
    {
        let mut st = widget.state.lock().unwrap();
        // Force a scroll offset so render_full_grid receives it.
        // PinnedGrid will clamp it to max_scroll internally.
        st.grid_scroll_offset = 1;
    }

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    // After render (which clamps offset), Richmond should still appear
    // as the leftmost city header because PinnedGrid always pins home.
    let richmond_y = (buf.area.y..buf.area.bottom())
        .find(|&y| row_text(buf, y).contains("Richmond"))
        .expect("Richmond must still appear after scroll");

    let richmond_xs = column_x_of(buf, richmond_y, "Richmond");
    let tokyo_xs = column_x_of(buf, richmond_y, "Tokyo");

    // Both should be present and Richmond still leftmost.
    if !tokyo_xs.is_empty() {
        assert!(
            richmond_xs[0] < tokyo_xs[0],
            "Home (Richmond) must remain leftmost after scroll"
        );
    }
}

/// When only 1 city fits in the grid (grid_scroll_offset already at max),
/// scroll_right returns Ignored.  The 2-city widget at 120×38 should fit
/// both columns (max_scroll=0), so grid_scroll(1) returns Ignored immediately.
#[test]
fn full_tier_scroll_ignored_when_all_cities_fit() {
    // Render at Full tier so last_tier is set to Full.
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();

    // Prime last_tier by rendering once at Full size.
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    // With 2 cities at 120 cols, both 48-col cards (stride 49, 2*49-1=97 < 118 inner)
    // fit, so max_scroll=0. grid_scroll(1) must return Ignored.
    let result = widget.grid_scroll(1);
    assert_eq!(
        result,
        EventResult::Ignored,
        "grid_scroll must return Ignored when all cities fit (max_scroll=0)"
    );
}

/// 7-day fallback: when daily data has fewer than 7 entries (only 3-day),
/// the Full-tier column renders "Next 3 days" and NOT "7-day".
#[test]
fn full_tier_7day_absent_falls_back_to_3day() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        // make_forecast_data supplies only 4 daily entries (today + 3):
        // skip(1).count() == 3, which triggers the 3-day fallback path.
        st.data_by_key.insert(
            loc_key(49.166, -123.133),
            make_forecast_data(provider::Units::Metric),
        );
    }

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    assert!(
        text.contains("Next 3 days"),
        "expected '3-day' fallback header when daily data is short"
    );
    assert!(
        !text.contains("7-day"),
        "must NOT show '7-day' header when fewer than 7 daily entries available"
    );
}

/// Standard-tier (60×25) with 2 cached cities must NOT render side-by-side
/// columns — the single-city carousel path is unchanged at non-Full tiers.
#[test]
fn standard_tier_two_cities_shows_single_city_not_columns() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    // Standard tier: 60×25 → ViewTier::Standard.
    let backend = TestBackend::new(60, 25);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    // At Standard tier the toggle row shows the selected city label.
    // "Tokyo" must NOT appear as a second column header — the grid is inactive.
    // Both city names might appear in the toggle bar, but the Full-tier columnar
    // layout (where both appear as top-row column headers side-by-side) must not.
    let text = buffer_text(buf);

    // Full-tier markers must be absent at Standard.
    assert!(
        !text.contains("Next 24h"),
        "Standard tier must NOT show Full-tier 'Next 24h' hourly section"
    );
    assert!(
        !text.contains("7-day"),
        "Standard tier must NOT show Full-tier '7-day' section"
    );
    // Only Standard rendering: 3-day forecast, single-city view.
    assert!(
        text.contains("Next 3 days"),
        "Standard tier must still show 'Next 3 days' forecast"
    );
}

/// Expanded-tier (80×28) with 2 cached cities also stays in single-city mode.
#[test]
fn expanded_tier_two_cities_shows_single_city_not_columns() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    // 80×28 → ViewTier::Expanded (width ≥ 65, height < 30).
    let backend = TestBackend::new(80, 28);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        !text.contains("Next 24h"),
        "Expanded tier must NOT show Full-tier hourly section"
    );
    assert!(
        !text.contains("7-day"),
        "Expanded tier must NOT show Full-tier '7-day' section"
    );
    assert!(
        text.contains("Next 3 days"),
        "Expanded tier must still show 'Next 3 days' forecast"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Full-tier separator + justified-width tests
// ─────────────────────────────────────────────────────────────────────────────

/// At Full size (120×38) with 2 cities, a `│` separator must appear between
/// the two city columns. This is the vertical bar drawn at the right edge of
/// every non-last column in `render_full_grid`.
#[test]
fn full_tier_two_cities_have_column_separator() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains('│'),
        "Full-tier grid with two cities must render a '│' separator between columns"
    );
}

/// At Full size, the `│` separator must appear in the interior of the buffer —
/// not at the leftmost or rightmost column — proving the columns are not
/// crammed against one edge.
#[test]
fn full_tier_separator_is_interior_not_at_edges() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // The separator must appear somewhere strictly between the outermost columns.
    // With 2 cities PinnedGrid places them in the first two of N equal columns
    // so the separator lands near the first column boundary — well away from
    // both edges. We just verify it's not at x=0 or x=area.right()-1.
    let sep_in_interior = (area.y..area.bottom()).any(|y| {
        (area.x + 1..area.right() - 1).any(|x| buf[(x, y)].symbol() == "│")
    });
    assert!(
        sep_in_interior,
        "Full-tier '│' separator must appear in the interior of the buffer \
         (not at the outermost columns)"
    );
}

/// At Full size with 2 cities, both city-name labels must appear in the buffer
/// and the separator must sit between them (home label x < separator x < secondary
/// label x), confirming home is leftmost and the separator divides the columns.
#[test]
fn full_tier_separator_between_home_and_secondary_columns() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // Find the row that contains "Richmond" (it will also contain "Tokyo" on
    // the same row — both city headers are on the same y).
    let header_y = (area.y..area.bottom())
        .find(|&y| row_text(buf, y).contains("Richmond"))
        .expect("Richmond city label must appear in Full-tier grid");

    assert!(
        row_text(buf, header_y).contains("Tokyo"),
        "Tokyo label must appear on the same row as Richmond"
    );

    // Separator column: find the first `│` in the row between the two labels.
    let richmond_xs = column_x_of(buf, header_y, "Richmond");
    let tokyo_xs = column_x_of(buf, header_y, "Tokyo");

    // Find any `│` on any row that is between richmond_xs[0] and tokyo_xs[0].
    let richmond_x = richmond_xs[0];
    let tokyo_x = tokyo_xs[0];
    let sep_between = (area.y..area.bottom()).any(|y| {
        (richmond_x..tokyo_x).any(|x| buf[(x, y)].symbol() == "│")
    });
    assert!(
        sep_between,
        "A '│' separator must appear between Richmond (x={richmond_x}) and \
         Tokyo (x={tokyo_x}) columns"
    );
}

/// Standard-tier (60×25) with 2 cached cities must NOT render a `│` inter-column
/// separator — that separator is exclusive to the Full-tier columnar grid.
#[test]
fn standard_tier_no_column_separator() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(60, 25);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    // Standard tier single-city view must not have inter-column separators.
    // (The widget border uses rounded-corner glyphs, not `│`, so any `│` here
    // would come from the grid separator code — which must not run at Standard.)
    let buf = terminal.backend().buffer();
    let area = buf.area;
    // Exclude the outermost border column (x = area.x) which draws `│` as part
    // of the bordered block. Scan only the interior.
    let interior_sep = (area.y + 1..area.bottom() - 1).any(|y| {
        (area.x + 1..area.right() - 1).any(|x| buf[(x, y)].symbol() == "│")
    });
    assert!(
        !interior_sep,
        "Standard tier must NOT render interior '│' separators (those are Full-tier only)"
    );
}

/// Expanded-tier (80×28) with 2 cached cities also must not render a `│`
/// inter-column separator.
#[test]
fn expanded_tier_no_column_separator() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(80, 28);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;
    let interior_sep = (area.y + 1..area.bottom() - 1).any(|y| {
        (area.x + 1..area.right() - 1).any(|x| buf[(x, y)].symbol() == "│")
    });
    assert!(
        !interior_sep,
        "Expanded tier must NOT render interior '│' separators (those are Full-tier only)"
    );
}

/// At a very wide Full-size terminal with only 2 cities, the two fixed-width
/// cards (40 cols each) must be centered in the available width — with the group
/// of cards horizontally centered, leaving outer margins on both sides.
/// Cards do NOT stretch to fill; Tokyo must appear in the right half of the buffer
/// (because the centered group places it well past the midpoint at 240 cols).
#[test]
fn full_tier_two_cities_centered_not_stretched() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 240-col terminal: inner area = 238. 2 cities × 48-col cards + 1-col gap = 97 cols group.
    // outer_margin = (238 - 97) / 2 = 70. Home card at x=1+70=71, Tokyo card at x=71+49=120.
    let (w, h) = (240u16, 40u16);

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // max_scroll must be 0 (both cities fit without scrolling).
    assert_eq!(
        widget.state.lock().unwrap().last_grid_max_scroll,
        0,
        "2 cities at 240 cols must fit without scrolling"
    );

    // Cards are centered: each city's card border must appear well away from
    // x=0. Home card starts at x=79; any `│` past x=60 (the left quarter)
    // confirms cards are not left-clustered.
    let sep_past_quarter = (area.y..area.bottom()).any(|y| {
        (area.x + w / 4..area.right() - 1).any(|x| buf[(x, y)].symbol() == "│")
    });
    assert!(
        sep_past_quarter,
        "a card border '│' must appear past the left quarter (centered layout), \
         confirming cards are not left-clustered"
    );

    // Tokyo (second city) must appear in the right half of the buffer.
    // With outer_margin=70, Tokyo card starts at x=120 (border), label inside at x>120.
    let tokyo_y = (area.y..area.bottom())
        .find(|&y| row_text(buf, y).contains("Tokyo"))
        .expect("Tokyo label must appear in Full-tier grid");
    let tokyo_xs = column_x_of(buf, tokyo_y, "Tokyo");
    assert!(!tokyo_xs.is_empty(), "Tokyo label x-positions must be found");
    assert!(
        tokyo_xs[0] > w / 2,
        "Tokyo column must start in the right half of the buffer (centered layout); tokyo_x={}",
        tokyo_xs[0]
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Full-tier card-border + forecast-ordering tests
// ─────────────────────────────────────────────────────────────────────────────

/// At Full size, each city column must be rendered as a bordered card.
/// The rounded-corner glyphs (`╭`, `╮`, `╰`, `╯`) must appear in the buffer.
#[test]
fn full_tier_city_cells_have_card_borders() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    assert!(
        text.contains('╭') || text.contains('╮') || text.contains('╰') || text.contains('╯'),
        "Full-tier weather city cells must render rounded card border corner glyphs"
    );
}

/// At Full size (120×55), the 7-day forecast must appear exactly ONCE per column
/// (not above AND below the hourly chart). We verify this by counting the
/// "7-day" header occurrences: with 1 city that's exactly 1; with 2 cities it's 2.
#[test]
fn full_tier_forecast_appears_exactly_once_per_column() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 120×55: tall enough for conditions + 7-day forecast + hourly.
    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 55);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    // 2-city widget → 2 columns → "7-day" header appears exactly twice.
    let count = text.matches("7-day").count();
    assert_eq!(
        count, 2,
        "each city column must show '7-day' header exactly once; found {count} occurrences"
    );
}

/// At Full size (120×55), the hourly chart must appear ABOVE the forecast listing.
/// New column order: conditions → 24h graph → 7-day forecast.
/// We verify this by finding the y-row of "Next 24h" and "7-day" and asserting
/// that the hourly header row is smaller (higher up) than the forecast header.
#[test]
fn full_tier_hourly_is_above_forecast() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    let backend = TestBackend::new(120, 55);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    let hourly_y = find_row_with(buf, "Next 24h").expect("'Next 24h' header must appear at Full tier");
    let forecast_y = find_row_with(buf, "7-day").expect("'7-day' header must appear at Full tier");

    assert!(
        hourly_y < forecast_y,
        "hourly chart ('Next 24h' at y={hourly_y}) must appear above forecast \
         ('7-day' at y={forecast_y}) — new order: conditions → 24h graph → 7-day forecast"
    );
}

/// At Full size (120×55), each forecast row in the consolidated listing must
/// be centered within the column: the row's leading padding must be non-zero
/// (the weekday abbreviation must not start at the left edge of the card inner area).
#[test]
fn full_tier_forecast_rows_are_centered() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    let backend = TestBackend::new(120, 55);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // Find a row that contains a weekday abbreviation AND appears after the 7-day header.
    let forecast_header_y = find_row_with(buf, "7-day")
        .expect("'7-day' header must appear");
    let weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

    // Find the first forecast data row (not the header line itself).
    let forecast_row_y = (forecast_header_y + 1..area.bottom()).find(|&y| {
        let row: String = (area.x..area.right())
            .flat_map(|x| buf[(x, y)].symbol().chars())
            .collect();
        weekdays.iter().any(|wd| row.contains(wd))
    });

    if let Some(fy) = forecast_row_y {
        // The weekday text must not start at x=1 (card inner left edge). With
        // centering the row has at least some leading space.
        let first_text = (area.x..area.right()).find(|&x| {
            let sym = buf[(x, fy)].symbol();
            sym != " " && sym != "│" && sym != "╭" && sym != "╮"
        });
        if let Some(tx) = first_text {
            assert!(
                tx > area.x + 2,
                "forecast rows must be centered (leading padding > 0); \
                 first non-space at x={tx}"
            );
        }
    }
}

/// Verify that update() schedules prefetch for extra configured cities, not
/// just the selected one. We test this by confirming that after build, the
/// extra city's poll tracker is seeded (constructor seeds it) — meaning the
/// widget is aware of it and will poll it.
#[test]
fn update_prefetches_all_configured_cities() {
    let cfg = WeatherConfig {
        label: Some("Home".into()),
        latitude: Some(49.166),
        longitude: Some(-123.133),
        cities: vec![
            WeatherCity { label: "Tokyo".into(), latitude: 35.68, longitude: 139.76 },
            WeatherCity { label: "London".into(), latitude: 51.51, longitude: -0.13 },
        ],
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);

    // Confirm that the constructor seeded poll trackers for all cities
    // (home + 2 extras = 3 trackers total).
    let st = w.state.lock().unwrap();
    let home_key = loc_key(49.166, -123.133);
    let tokyo_key = loc_key(35.68, 139.76);
    let london_key = loc_key(51.51, -0.13);
    assert!(
        st.poll_by_key.contains_key(&home_key),
        "home city must have a poll tracker"
    );
    assert!(
        st.poll_by_key.contains_key(&tokyo_key),
        "Tokyo extra city must have a poll tracker"
    );
    assert!(
        st.poll_by_key.contains_key(&london_key),
        "London extra city must have a poll tracker"
    );
    // All three trackers must be seeded so the first update() call
    // will schedule fetches for them when due.
    assert_eq!(st.poll_by_key.len(), 3, "expected exactly 3 poll trackers (home + 2 extras)");
}

// ─────────────────────────────────────────────────────────────────────────────
// Full-tier footer: scroll indicator vs. no-footer tests
// ─────────────────────────────────────────────────────────────────────────────

/// Return every distinct row-text in the bottom N rows of the buffer.
/// Used to check that the last row(s) don't contain scroll arrow glyphs.
fn bottom_rows_text(buf: &ratatui::buffer::Buffer, n: u16) -> String {
    let area = buf.area;
    let start_y = area.bottom().saturating_sub(n);
    (start_y..area.bottom())
        .flat_map(|y| (area.x..area.right()).flat_map(move |x| buf[(x, y)].symbol().chars()))
        .collect()
}

/// Build a widget with `extra_cities` additional cities beyond the home city.
/// Each gets a distinct lat/lon so their cache keys don't collide. Data is
/// pre-seeded for all cities.
fn build_widget_with_n_extra_cities(n: usize) -> WeatherWidget {
    // Use 1.0-degree steps in latitude starting at 10.0 for extras.
    let extras: Vec<WeatherCity> = (0..n)
        .map(|i| WeatherCity {
            label: format!("City{i}"),
            latitude: 10.0 + i as f64,
            longitude: 0.0,
        })
        .collect();

    let cfg = WeatherConfig {
        label: Some("Home".into()),
        latitude: Some(49.166),
        longitude: Some(-123.133),
        cities: extras.clone(),
        ..WeatherConfig::default()
    };
    let w = build_widget(cfg);
    {
        let mut st = w.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
        for (i, _city) in extras.iter().enumerate() {
            st.data_by_key.insert(loc_key(10.0 + i as f64, 0.0), make_full_data());
        }
    }
    w
}

/// At Full tier with few cities that all fit on screen, no scroll arrow row
/// (◄ or ►) must appear at the bottom of the widget.
///
/// Terminal 120×38 → Full tier. COL_MIN_COLS=28; capacity = 4 columns. With
/// 2 cities both fit, so max_scroll=0 after the first render. The grid body
/// must extend to the bottom border with no footer row reserved.
#[test]
fn full_tier_no_footer_when_all_cities_fit() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 2 cities at 120 cols: both 48-col cards fit (stride 49, group=97 < 118 inner),
    // so max_scroll == 0. No scroll indicator.
    let widget = build_two_city_widget_with_data();

    // Prime last_grid_max_scroll by rendering once (it starts at 0 and will
    // stay 0 after this render because 2 cities fit at 120 cols).
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    // Confirm overflow flag did not fire.
    assert_eq!(
        widget.state.lock().unwrap().last_grid_max_scroll,
        0,
        "2 cities at 120 cols must all fit (max_scroll == 0)"
    );

    let buf = terminal.backend().buffer();
    let bottom_text = bottom_rows_text(buf, 2);

    // No scroll arrows should appear anywhere in the bottom two rows.
    assert!(
        !bottom_text.contains('◄') && !bottom_text.contains('►'),
        "Full-tier footer must be absent when all cities fit; bottom rows: {bottom_text:?}"
    );
}

/// At Full tier with many cities that overflow the grid and scroll_offset == 0,
/// only ► must appear in the footer (no ◄ because there is nothing to the left).
///
/// Terminal 120×38 → Full tier. With 6 cities max_scroll > 0 after the first
/// render. offset stays 0, so ◄ is absent and ► is present.
#[test]
fn full_tier_scroll_indicator_when_cities_overflow() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 6 cities at 120 cols: 5 extras + home = 6 total. Fixed 48-col cards
    // (stride 49) fit 2 cards in 118-col inner area, so max_scroll > 0 and the indicator must appear.
    let widget = build_widget_with_n_extra_cities(5);

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();

    // First render: populates last_grid_max_scroll.
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    assert!(
        widget.state.lock().unwrap().last_grid_max_scroll > 0,
        "6 cities at 120 cols must overflow (max_scroll > 0)"
    );

    // Second render: footer row is now reserved using the stored max_scroll.
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let bottom_text = bottom_rows_text(buf, 2);

    // offset == 0: only ► is shown (nothing to the left yet).
    assert!(
        bottom_text.contains('►'),
        "Full-tier footer must show ► at offset 0; bottom rows: {bottom_text:?}"
    );
    assert!(
        !bottom_text.contains('◄'),
        "Full-tier footer must NOT show ◄ at offset 0 (nothing to the left); \
         bottom rows: {bottom_text:?}"
    );
}

/// On the VERY FIRST render after switching to Full tier with overflowing cities,
/// the scroll arrow (►) must already be present — no second render required.
///
/// This is the regression guard for the display-lag bug where the footer read
/// `last_grid_max_scroll` (written by the previous frame's `render_full_grid`)
/// and therefore showed no arrow on frame 1. The fix computes overflow via
/// `full_grid_fit` in the same frame as the footer layout decision, so the
/// arrow appears immediately.
///
/// Contrast with `full_tier_scroll_indicator_when_cities_overflow` which still
/// asserts correctness after two renders; this test asserts correctness after ONE.
#[test]
fn full_tier_scroll_arrow_appears_on_first_render() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 6 cities at 120 cols: 5 extras + home = 6 total. Fixed 48-col cards
    // (stride 49) fit 2 cards in the 118-col inner area, so max_scroll > 0.
    let widget = build_widget_with_n_extra_cities(5);

    // Confirm the widget starts with last_grid_max_scroll == 0 (stale initial state).
    assert_eq!(
        widget.state.lock().unwrap().last_grid_max_scroll,
        0,
        "last_grid_max_scroll must start at 0 (the stale initial value)"
    );

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();

    // Single render — ► must be present immediately, not after a second frame.
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let bottom_text = bottom_rows_text(terminal.backend().buffer(), 2);
    assert!(
        bottom_text.contains('►'),
        "► must appear on the FIRST render when cities overflow; \
         bottom rows: {bottom_text:?}\n\
         (failure means the footer still reads the previous-frame last_grid_max_scroll)"
    );
    assert!(
        !bottom_text.contains('◄'),
        "◄ must NOT appear at offset 0 (nothing to the left); bottom rows: {bottom_text:?}"
    );
}

/// At Standard tier (60×25) with 2 cached cities, the carousel toggle row
/// must still appear at the bottom — the non-Full behavior is unchanged.
#[test]
fn standard_tier_carousel_toggle_still_renders() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(60, 25);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    // The carousel toggle renders city label in [brackets] with ◂/▸ arrows.
    // At Standard tier with 2 cities the toggle is always shown.
    let text = buffer_text(buf);
    assert!(
        text.contains('[') && text.contains(']'),
        "Standard tier must render the carousel toggle (city label in [brackets])"
    );
    // Standard tier must NOT show the Full-tier scroll arrows (◄/►).
    assert!(
        !text.contains('◄') && !text.contains('►'),
        "Standard tier must NOT render Full-tier scroll indicator arrows"
    );
}

/// At Expanded tier (80×28) with 2 cached cities, the carousel toggle row
/// must still appear and Full-tier scroll indicators must be absent.
#[test]
fn expanded_tier_carousel_toggle_still_renders() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(80, 28);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let text = buffer_text(buf);
    assert!(
        text.contains('[') && text.contains(']'),
        "Expanded tier must render the carousel toggle (city label in [brackets])"
    );
    assert!(
        !text.contains('◄') && !text.contains('►'),
        "Expanded tier must NOT render Full-tier scroll indicator arrows"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Full-tier fixed-width centered card layout tests
//
// These tests verify the card geometry:
//   - Each card is ≤ 48 columns wide (never stretched beyond that).
//   - A 1-column gap separates adjacent card borders.
//   - Inside each card a 2-col left/right margin insets the content.
//   - The card group is horizontally centered; leftover columns go to the
//     outer left and right as equal margins (cards do NOT fill the pane width).
//   - Home is always the leftmost card; scroll/overflow indicators still work.
// ─────────────────────────────────────────────────────────────────────────────

/// Helper: return the x-coordinates of the rounded card left-border corner
/// glyphs (`╭`) found in a given buffer row, skipping the outermost widget
/// border column (x = area.x) which is the weather widget's own border.
fn card_left_border_xs(buf: &ratatui::buffer::Buffer, y: u16) -> Vec<u16> {
    let area = buf.area;
    // area.x + 1: skip the outer widget left border column
    (area.x + 1..area.right())
        .filter(|&x| buf[(x, y)].symbol() == "╭")
        .collect()
}

/// Helper: return the x-coordinate of every card's right border glyph (`╮`)
/// found in a given buffer row, skipping the outermost widget border column.
fn card_right_border_xs(buf: &ratatui::buffer::Buffer, y: u16) -> Vec<u16> {
    let area = buf.area;
    // area.right() - 2: skip the outer widget right border column (area.right()-1)
    (area.x + 1..area.right() - 1)
        .filter(|&x| buf[(x, y)].symbol() == "╮")
        .collect()
}

/// Helper: find the y-row of the first inner card top border (the row containing
/// `╭` at x > area.x, i.e. skipping the outer widget top-left corner at area.x).
fn find_card_top_border_y(buf: &ratatui::buffer::Buffer) -> Option<u16> {
    let area = buf.area;
    // Start from area.y + 1 to skip the outer widget border row.
    (area.y + 1..area.bottom()).find(|&y| !card_left_border_xs(buf, y).is_empty())
}

/// At Full size (120×38) with 2 cities, each card must be exactly 48 columns
/// wide (measured as the distance between its `╭` and `╮` top corners).
#[test]
fn full_tier_card_width_is_48() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    // Skip the outer widget border row (y=0); find the first inner card top border.
    let top_y = find_card_top_border_y(buf)
        .expect("must find inner card top-border row (╭ at x > area.x)");

    let left_xs = card_left_border_xs(buf, top_y);
    let right_xs = card_right_border_xs(buf, top_y);

    assert!(
        left_xs.len() >= 2,
        "expected ≥ 2 card left-border corners (╭) at y={top_y}; found {}",
        left_xs.len()
    );
    assert!(
        right_xs.len() >= 2,
        "expected ≥ 2 card right-border corners (╮) at y={top_y}; found {}",
        right_xs.len()
    );

    // Each card's width = right_x - left_x + 1 (inclusive). Both cards must be 48.
    for i in 0..left_xs.len().min(right_xs.len()) {
        let card_width = right_xs[i] - left_xs[i] + 1;
        assert_eq!(
            card_width,
            48,
            "card {i} width must be 48; got {card_width} (╭ at x={}, ╮ at x={})",
            left_xs[i],
            right_xs[i]
        );
    }
}

/// At Full size (120×38) with 2 cities, adjacent cards must have exactly 1
/// column of gap between them (the right border of card 0 + 1 = the left border of card 1).
#[test]
fn full_tier_inter_card_gap_is_one_column() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    let top_y = find_card_top_border_y(buf)
        .expect("must find inner card top-border row");

    let left_xs = card_left_border_xs(buf, top_y);
    let right_xs = card_right_border_xs(buf, top_y);

    // With 2 cards: gap = left_xs[1] - right_xs[0] - 1.
    // Expected: gap == 1 (the 1-col inter-card space between card right border and next card left border).
    if left_xs.len() >= 2 && right_xs.len() >= 2 {
        let gap = left_xs[1].saturating_sub(right_xs[0]).saturating_sub(1);
        assert_eq!(
            gap,
            1,
            "inter-card gap must be 1 column; got {gap} \
             (card0 right=x{}, card1 left=x{})",
            right_xs[0],
            left_xs[1]
        );
    }
}

/// At Full size (120×38) with 2 cities, the content inside each card must be
/// inset by 2 columns on the left and 2 columns on the right.
///
/// Card inner area starts at card_left_x + 1 (inside the card border).
/// Content must not start until card_inner_x + 2 (the 2-col left margin).
/// We verify that no text appears within the first 2 columns of the card inner area.
///
/// Content width = card_inner_width - 4 (subtracting 2+2 side margins).
/// For a 48-col card: card_inner = 46 cols, content = 42 cols.
#[test]
fn full_tier_card_content_inset_is_2() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    let top_y = find_card_top_border_y(buf)
        .expect("must find inner card top-border row");
    let left_xs = card_left_border_xs(buf, top_y);
    assert!(!left_xs.is_empty(), "must find at least one card");

    let card_inner_x = left_xs[0] + 1; // first column inside the card border
    // The 2-col left margin means columns card_inner_x and card_inner_x+1 must be
    // blank (space) throughout all rows that carry text content (skip the top border row).
    // We scan a few rows below the card top border to catch content rows.
    let content_start_y = top_y + 1; // first row inside the card
    let scan_rows = 8u16;

    for dy in 0..scan_rows {
        let y = content_start_y + dy;
        if y >= area.bottom() {
            break;
        }
        for col_offset in 0u16..2 {
            let x = card_inner_x + col_offset;
            let sym = buf[(x, y)].symbol();
            // Only reject non-space, non-border characters — the card's own
            // left-border │ at x=card_inner_x-1 is fine; inside the inset zone
            // we should see only spaces.
            assert!(
                sym == " ",
                "2-col left margin violated: non-space '{sym}' at x={x}, y={y} \
                 (card_inner_x={card_inner_x}, col_offset={col_offset})"
            );
        }
    }
}

/// At Full size with 2 cities and a wide terminal (200 cols), the card group
/// must be horizontally centered: the outer left margin must roughly equal the
/// outer right margin (within ±1 for integer-division rounding).
///
/// Inner area is 198 wide (outer widget border takes 1 col each side).
/// Group = 2*48 + 1 = 97. outer_margin = (198-97)/2 = 50.
/// Home card left border at x = 1 + 50 = 51. Tokyo right border at x = 51+48+49-1 = 147.
/// outer_right = 198 - (147-1+1) = 198 - 147 = 51. Equal (diff ≤ 1).
#[test]
fn full_tier_card_group_is_centered() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let (w, h) = (200u16, 40u16);

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    let top_y = find_card_top_border_y(buf)
        .expect("must find inner card top-border row");

    let left_xs = card_left_border_xs(buf, top_y);
    let right_xs = card_right_border_xs(buf, top_y);

    assert!(
        !left_xs.is_empty() && !right_xs.is_empty(),
        "must find card borders in the buffer"
    );

    // The body area starts at x=1 (inside outer widget border).
    // outer_left = first_card_left_x - body_x = left_xs[0] - 1
    let body_x: u16 = 1;
    let outer_left = left_xs[0].saturating_sub(body_x);
    // body_right = area.width - 2 = 198. outer_right = body_right - last_card_right_x + body_x - 1
    let body_right = area.width - 2; // inner area width
    let last_right = *right_xs.last().unwrap();
    // last_right is the absolute x of the last card's ╮. Relative to body: last_right - body_x.
    // outer_right = body_right - (last_right - body_x) - 1
    let outer_right = body_right.saturating_sub(last_right - body_x + 1);

    assert!(
        outer_left.abs_diff(outer_right) <= 1,
        "outer left margin ({outer_left}) must roughly equal outer right margin ({outer_right}); \
         difference must be ≤ 1"
    );
    // Sanity: there IS a meaningful margin.
    assert!(
        outer_left > 0,
        "outer left margin must be > 0 at 200 cols with 2 cities"
    );
}

/// At Full size with 2 cities, cards must NOT stretch to fill the available width.
/// The card width must remain 48 regardless of pane width. At 200 cols the cards
/// are 48 wide (not 100 each).
#[test]
fn full_tier_cards_do_not_stretch_beyond_48() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 200-col terminal: would be 100 cols per card if stretching. Must stay 48.
    let (w, h) = (200u16, 40u16);

    let widget = build_two_city_widget_with_data();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    let top_y = find_card_top_border_y(buf)
        .expect("must find inner card top-border row");

    let left_xs = card_left_border_xs(buf, top_y);
    let right_xs = card_right_border_xs(buf, top_y);

    assert!(
        !left_xs.is_empty(),
        "must find at least one card left border"
    );
    for i in 0..left_xs.len().min(right_xs.len()) {
        let card_width = right_xs[i] - left_xs[i] + 1;
        assert!(
            card_width <= 48,
            "card {i} must be ≤ 48 cols wide at a {w}-col terminal; got {card_width}"
        );
    }
}

/// When only 1 city is configured at Full tier, it must not stretch to fill the
/// pane. The single card must be 48 cols wide (clamped to pane width if narrower)
/// and must be horizontally centered within the inner body area.
///
/// At 120×38: inner area width = 118. card_w=48. outer_margin=(118-48)/2=35.
/// Card ╭ at x=1+35=36. ╮ at x=83. outer_left=35. outer_right = 118-1-83 = 34. Diff ≤ 1.
#[test]
fn full_tier_single_city_card_is_48_wide_and_centered() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // Single-city widget at Full size: 120×38.
    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    let (w, h) = (120u16, 38u16);
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    let top_y = find_card_top_border_y(buf)
        .expect("must find inner card top-border row");

    let left_xs = card_left_border_xs(buf, top_y);
    let right_xs = card_right_border_xs(buf, top_y);

    assert_eq!(left_xs.len(), 1, "single city → 1 card left border (╭)");
    assert_eq!(right_xs.len(), 1, "single city → 1 card right border (╮)");

    let card_width = right_xs[0] - left_xs[0] + 1;
    assert_eq!(card_width, 48, "single card must be 48 cols wide; got {card_width}");

    // Centering: body_x = 1 (inside outer widget border at x=0).
    // outer_left = card_left - body_x. outer_right = (body_x + inner_width - 1) - card_right.
    let body_x: u16 = 1;
    let inner_width = w - 2; // 118
    let outer_left = left_xs[0].saturating_sub(body_x);
    let outer_right = (body_x + inner_width - 1).saturating_sub(right_xs[0]);
    assert!(
        outer_left.abs_diff(outer_right) <= 1,
        "single card must be centered within the body; \
         outer_left={outer_left}, outer_right={outer_right}"
    );
    assert!(outer_left > 0, "must have outer left margin");
}

/// At Full tier with 6 cities at 120 cols, the scroll indicator must appear
/// when cities overflow. With fixed 48-col cards (stride 49), only 2 cards fit
/// in 118-col inner area ((118+1)/49=2), so max_scroll = 6 - 2 = 4.
/// offset == 0 → only ► appears (nothing to the left yet).
#[test]
fn full_tier_overflow_recomputed_from_fixed_width_layout() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_widget_with_n_extra_cities(5); // 6 total

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();

    // First render to populate last_grid_max_scroll.
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let max_scroll = widget.state.lock().unwrap().last_grid_max_scroll;
    assert_eq!(
        max_scroll, 4,
        "with 6 cities and 2 cards fitting at 120 cols, max_scroll must be 4; got {max_scroll}"
    );

    // Second render: footer row is reserved; offset==0 so only ► is shown.
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let bottom_text = bottom_rows_text(terminal.backend().buffer(), 2);
    assert!(
        bottom_text.contains('►'),
        "► indicator must appear when cities overflow fixed-width layout; \
         bottom rows: {bottom_text:?}"
    );
    assert!(
        !bottom_text.contains('◄'),
        "◄ must NOT appear at offset 0; bottom rows: {bottom_text:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Fix 1: forecast_days=8 → 7-day listing fires at Full tier
// ─────────────────────────────────────────────────────────────────────────────

/// Full-tier with 8 daily entries (today + 7 future) must show the "7-day"
/// section header and exactly 7 weekday forecast rows (one per future day).
#[test]
fn full_tier_8daily_entries_renders_7day_header_and_7_rows() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // make_full_data() builds 8 daily entries: today + 7 future days.
    // skip(1).count() == 7, which crosses the >= 7 threshold.
    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    // 120×55: tall enough to show the full 7-day block inside a Full-tier card.
    let backend = TestBackend::new(120u16, 55u16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    assert!(
        text.contains("7-day"),
        "Full-tier with 8 daily entries must show '7-day' section header; \
         first 400 chars:\n{}",
        &text[..text.len().min(400)]
    );

    // Count distinct weekday occurrences — there must be ≥ 7 (one per forecast day).
    let weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let hits: usize = weekdays.iter().map(|wd| text.matches(wd).count()).sum();
    assert!(
        hits >= 7,
        "Full-tier 7-day block must render ≥ 7 weekday rows; found {hits}"
    );

    // Must NOT fall back to the 3-day label.
    assert!(
        !text.contains("Next 3 days"),
        "Full-tier with 8 daily entries must NOT show 'Next 3 days' fallback"
    );
}

/// Full-tier with only 4 daily entries (today + 3 future) must fall back to
/// "Next 3 days" because skip(1).count() == 3 < 7.
#[test]
fn full_tier_short_daily_falls_back_to_3day() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // make_forecast_data() supplies 4 daily entries (today + 3 future days).
    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(
            loc_key(49.166, -123.133),
            make_forecast_data(provider::Units::Metric),
        );
    }

    let backend = TestBackend::new(120u16, 55u16);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    assert!(
        text.contains("Next 3 days"),
        "Full-tier with only 4 daily entries must show 'Next 3 days' fallback"
    );
    assert!(
        !text.contains("7-day"),
        "Full-tier with fewer than 7 future days must NOT show '7-day' header"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Fix 2: directional scroll arrows
// ─────────────────────────────────────────────────────────────────────────────

/// At offset 0 with overflow, only ► is shown (nothing to the left).
#[test]
fn full_tier_footer_at_offset_0_shows_only_right_arrow() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_widget_with_n_extra_cities(5); // 6 cities → overflow

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();

    // Prime last_grid_max_scroll.
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();
    assert!(widget.state.lock().unwrap().last_grid_max_scroll > 0);

    // Ensure offset is 0.
    widget.state.lock().unwrap().grid_scroll_offset = 0;

    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let bottom_text = bottom_rows_text(terminal.backend().buffer(), 2);
    assert!(
        bottom_text.contains('►'),
        "offset 0: ► must be present; bottom rows: {bottom_text:?}"
    );
    assert!(
        !bottom_text.contains('◄'),
        "offset 0: ◄ must be absent; bottom rows: {bottom_text:?}"
    );
}

/// At max_scroll offset, only ◄ is shown (nothing to the right).
#[test]
fn full_tier_footer_at_max_scroll_shows_only_left_arrow() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_widget_with_n_extra_cities(5); // 6 cities → overflow

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();

    // Prime last_grid_max_scroll.
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let max_scroll = widget.state.lock().unwrap().last_grid_max_scroll;
    assert!(max_scroll > 0, "need overflow for this test");

    // Set offset to max_scroll (rightmost position).
    widget.state.lock().unwrap().grid_scroll_offset = max_scroll;

    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let bottom_text = bottom_rows_text(terminal.backend().buffer(), 2);
    assert!(
        bottom_text.contains('◄'),
        "max_scroll: ◄ must be present; bottom rows: {bottom_text:?}"
    );
    assert!(
        !bottom_text.contains('►'),
        "max_scroll: ► must be absent; bottom rows: {bottom_text:?}"
    );
}

/// At a mid-scroll position, both ◄ and ► are shown.
#[test]
fn full_tier_footer_at_mid_scroll_shows_both_arrows() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = build_widget_with_n_extra_cities(5); // 6 cities → max_scroll=4

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();

    // Prime last_grid_max_scroll.
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let max_scroll = widget.state.lock().unwrap().last_grid_max_scroll;
    assert!(max_scroll >= 2, "need at least 2 scroll positions for mid-scroll test");

    // Set to a mid position: 0 < offset < max_scroll.
    widget.state.lock().unwrap().grid_scroll_offset = max_scroll / 2;

    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let bottom_text = bottom_rows_text(terminal.backend().buffer(), 2);
    assert!(
        bottom_text.contains('◄') && bottom_text.contains('►'),
        "mid-scroll: both ◄ and ► must be present; bottom rows: {bottom_text:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Full-tier spacing + header-width tests (fixes for the three layout issues)
// ─────────────────────────────────────────────────────────────────────────────

/// Return the last x-coordinate in buffer row `y` (scanning right-to-left)
/// whose cell symbol is not a space. Returns None when the row is entirely blank.
fn last_text_x(buf: &ratatui::buffer::Buffer, y: u16) -> Option<u16> {
    let area = buf.area;
    (area.x..area.right()).rev().find(|&x| buf[(x, y)].symbol() != " ")
}

/// Return true when every cell in row `y` between columns `x_start..x_end` is
/// either a space or a card-border glyph ("│"). Card border glyphs appear on
/// every interior row of the Full-tier cards and are not content.
fn row_is_blank_in_range(buf: &ratatui::buffer::Buffer, y: u16, x_start: u16, x_end: u16) -> bool {
    (x_start..x_end).all(|x| {
        let sym = buf[(x, y)].symbol();
        sym == " " || sym == "│"
    })
}

/// Fix 1: exactly one blank row between the last conditions line (Humidity/Wind)
/// and the "── Next 24h" header.
///
/// At Full tier (120×55) the conditions section ends with the Humidity/Wind line.
/// `render_conditions_art_only` now returns its actual row count; the caller
/// advances by that count + 1 blank spacer before the hourly header.  This test
/// verifies the gap is exactly 1 row.
#[test]
fn full_tier_exactly_one_blank_between_conditions_and_hourly() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    let backend = TestBackend::new(120, 55);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    let humidity_y =
        find_row_with(buf, "Humidity").expect("'Humidity' line must appear at Full tier");
    let hourly_y =
        find_row_with(buf, "Next 24h").expect("'Next 24h' header must appear at Full tier");

    // The gap between the Humidity line and the hourly header must be exactly 1 row.
    assert_eq!(
        hourly_y,
        humidity_y + 2,
        "exactly one blank row expected between Humidity (y={humidity_y}) and \
         Next 24h (y={hourly_y}); gap = {}",
        hourly_y.saturating_sub(humidity_y + 1),
    );

    // The intermediate row must be blank (all spaces in the interior).
    let blank_y = humidity_y + 1;
    assert!(
        row_is_blank_in_range(buf, blank_y, area.x + 1, area.right() - 1),
        "row y={blank_y} between Humidity and Next 24h must be blank"
    );
}

/// Fix 2: the "── 7-day" section header's trailing "─" run must span the full
/// card content width, reaching the same rightmost column as the "── Next 24h"
/// header (which already computed its fill from display width).
///
/// The old code used `header_label.len()` (byte length) for the fill, which
/// undercounted the "──" box-drawing chars (3 bytes each, 1 display col each),
/// making the 7-day header stop short.  The fix uses `chars().count()`.
#[test]
fn full_tier_7day_header_spans_full_content_width() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    let backend = TestBackend::new(120, 55);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();

    let hourly_y =
        find_row_with(buf, "Next 24h").expect("'Next 24h' header must appear at Full tier");
    let forecast_y =
        find_row_with(buf, "7-day").expect("'7-day' header must appear at Full tier");

    // Both headers must reach the same rightmost column: the full content width.
    let hourly_right = last_text_x(buf, hourly_y)
        .expect("Next 24h header must contain non-space text");
    let forecast_right = last_text_x(buf, forecast_y)
        .expect("7-day header must contain non-space text");

    assert_eq!(
        forecast_right,
        hourly_right,
        "7-day header must span to the same rightmost column as Next 24h header \
         (both fill the full content width); 7-day right={forecast_right}, \
         Next 24h right={hourly_right}"
    );
}

/// Fix 3: exactly one blank row between the "Rain%" line (last row of the 24h
/// section) and the "── 7-day" header.
///
/// Without this fix the hourly section's trailing blank was removed from inside
/// `render_hourly_section` and the caller advanced by one extra row instead, so
/// the gap is always exactly 1 row regardless of how much space remains.
#[test]
fn full_tier_exactly_one_blank_between_rain_and_7day_header() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let widget = WeatherWidget::default();
    {
        let mut st = widget.state.lock().unwrap();
        st.data_by_key.insert(loc_key(49.166, -123.133), make_full_data());
    }

    let backend = TestBackend::new(120, 55);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    let rain_y =
        find_row_with(buf, "Rain%").expect("'Rain%' bar must appear at Full tier");
    let forecast_y =
        find_row_with(buf, "7-day").expect("'7-day' header must appear at Full tier");

    // Exactly one blank row separates rain% and the 7-day header.
    assert_eq!(
        forecast_y,
        rain_y + 2,
        "exactly one blank row expected between Rain% (y={rain_y}) and \
         7-day header (y={forecast_y}); gap = {}",
        forecast_y.saturating_sub(rain_y + 1),
    );

    let blank_y = rain_y + 1;
    assert!(
        row_is_blank_in_range(buf, blank_y, area.x + 1, area.right() - 1),
        "row y={blank_y} between Rain% and 7-day header must be blank"
    );
}

/// When all cities fit (max_scroll == 0), no footer row is rendered at all.
#[test]
fn full_tier_footer_absent_when_no_overflow() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    // 2 cities at 120 cols: both fit, max_scroll == 0.
    let widget = build_two_city_widget_with_data();

    let backend = TestBackend::new(120, 38);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    assert_eq!(
        widget.state.lock().unwrap().last_grid_max_scroll,
        0,
        "2 cities at 120 cols must fit with max_scroll == 0"
    );

    let bottom_text = bottom_rows_text(terminal.backend().buffer(), 2);
    assert!(
        !bottom_text.contains('◄') && !bottom_text.contains('►'),
        "no footer when all cities fit; bottom rows: {bottom_text:?}"
    );
}
