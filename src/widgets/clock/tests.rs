// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the clock widget. Split out of `mod.rs` to keep the
//! widget-entry file readable; everything else is unchanged.

use super::clock_view::{city_from_tz_name, day_night_icon};
use super::config::{label_from_iana_zone, render_clock_toml, SecondaryTimezone};
use super::state::Mode;
use super::timer::TimerPhase;
use super::*;
use crate::theme::ColorScheme;
use crate::ui::big_digits;
use crate::widgets::view_tier::{EXPANDED_MIN_W, FULL_MIN_H, FULL_MIN_W};
use crate::widgets::ViewTier;
use chrono::TimeZone;
use ratatui::{backend::TestBackend, Terminal};
use std::time::Duration;

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
        st.transient_tz = Some((
            "Berlin, Berlin, Germany".into(),
            "Berlin".into(),
            "Europe/Berlin".parse::<Tz>().unwrap(),
        ));
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
fn move_world_clock_selection_ignored_when_no_secondaries() {
    let w = build_widget(ClockConfig::default());
    assert_eq!(w.move_world_clock_selection(1), EventResult::Ignored);
    assert_eq!(w.move_world_clock_selection(-1), EventResult::Ignored);
    assert!(w.state.lock().unwrap().world_clock_selected.is_none());
}

#[test]
fn first_press_lands_on_first_secondary_without_transient() {
    let cfg = ClockConfig {
        secondary_timezones: vec![
            SecondaryTimezone {
                label: "Tokyo".into(),
                timezone: "Asia/Tokyo".into(),
            },
            SecondaryTimezone {
                label: "London".into(),
                timezone: "Europe/London".into(),
            },
        ],
        ..ClockConfig::default()
    };
    let w = build_widget(cfg);
    assert_eq!(w.move_world_clock_selection(1), EventResult::Handled);
    // No transient → entries = [primary, Tokyo, London]; first
    // selectable is idx 1.
    assert_eq!(w.state.lock().unwrap().world_clock_selected, Some(1));
    assert_eq!(w.move_world_clock_selection(1), EventResult::Handled);
    assert_eq!(w.state.lock().unwrap().world_clock_selected, Some(2));
    // Clamp at the bottom of the list.
    assert_eq!(w.move_world_clock_selection(1), EventResult::Handled);
    assert_eq!(w.state.lock().unwrap().world_clock_selected, Some(2));
    // Walk back up and clamp at the top of the selectable range.
    w.move_world_clock_selection(-1);
    w.move_world_clock_selection(-1);
    w.move_world_clock_selection(-1);
    assert_eq!(w.state.lock().unwrap().world_clock_selected, Some(1));
}

#[test]
fn first_press_lands_on_first_secondary_with_transient_skipping_local_and_lookup() {
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
        st.transient_tz = Some((
            "Berlin, Berlin, Germany".into(),
            "Berlin".into(),
            "Europe/Berlin".parse::<Tz>().unwrap(),
        ));
    }
    // Entries with transient = [Local, Berlin, Tokyo]; first
    // selectable should skip Local + Berlin and land at idx 2.
    assert_eq!(w.move_world_clock_selection(1), EventResult::Handled);
    assert_eq!(w.state.lock().unwrap().world_clock_selected, Some(2));
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

// ─────────────────────────────────────────────────────────────────────
// Responsive Full-tier view tests
//
// Each of the three clock modes (Clock / Stopwatch / Timer) has a richer
// view gated on ViewTier::Full.  The tests below use TestBackend renders
// to assert:
//
//   a) At Full size (FULL_MIN_W × FULL_MIN_H) the Full-tier path activates
//      and produces observable evidence (state change or buffer content).
//   b) At Standard (50 × 20) and Expanded (EXPANDED_MIN_W × 24) the
//      existing rendering is unchanged and no Full-tier content leaks.
//
// Size preconditions are checked with ViewTier::from_rect assertions so
// a future threshold change fails loudly here rather than silently
// producing wrong test conclusions.
// ─────────────────────────────────────────────────────────────────────

use crate::widgets::test_support::buffer_text;

/// Build a list of 15 valid secondary timezones spread across regions.
/// With the primary timezone that is 16 total world-clock entries —
/// enough to overflow the Standard and Expanded list views, which cap out
/// at ~6–10 visible rows, while the Full grid accommodates all of them.
#[cfg(test)]
fn many_secondaries() -> Vec<SecondaryTimezone> {
    [
        ("New York", "America/New_York"),
        ("London", "Europe/London"),
        ("Tokyo", "Asia/Tokyo"),
        ("Sydney", "Australia/Sydney"),
        ("Chicago", "America/Chicago"),
        ("Paris", "Europe/Paris"),
        ("Shanghai", "Asia/Shanghai"),
        ("Los Angeles", "America/Los_Angeles"),
        ("Berlin", "Europe/Berlin"),
        ("Mumbai", "Asia/Kolkata"),
        ("Denver", "America/Denver"),
        ("Auckland", "Pacific/Auckland"),
        ("Dubai", "Asia/Dubai"),
        ("Toronto", "America/Toronto"),
        ("Moscow", "Europe/Moscow"),
    ]
    .iter()
    .map(|(label, tz)| SecondaryTimezone {
        label: label.to_string(),
        timezone: tz.to_string(),
    })
    .collect()
}

// ── Clock mode ────────────────────────────────────────────────────────

/// Build a widget with a small number of secondary zones — enough to fit
/// all at once in the Full-tier big-digit grid so max_scroll == 0.
#[cfg(test)]
fn few_secondaries() -> Vec<SecondaryTimezone> {
    [("New York", "America/New_York"), ("London", "Europe/London")]
        .iter()
        .map(|(label, tz)| SecondaryTimezone {
            label: label.to_string(),
            timezone: tz.to_string(),
        })
        .collect()
}

/// At Full size with 3 zones (primary + 2 secondaries), the big-digit
/// grid renders. The buffer must contain '█' (big-digit full-block chars
/// from render_styled in Normal gradient mode) and must NOT contain the
/// "World Clocks" header text that the Standard/Expanded list path emits.
/// Also asserts max_scroll == 0 (all zones fit without scrolling).
#[test]
fn clock_full_tier_big_digit_grid_renders_and_no_list_header() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    // max_scroll == 0: 3 zones fit in the big-digit grid without scrolling.
    assert_eq!(
        widget.state.lock().unwrap().world_clock_max_scroll,
        0,
        "3 zones must fit without scrolling at Full: max_scroll must be 0"
    );

    let text = buffer_text(terminal.backend().buffer());

    // Big-digit full-block characters must appear in the buffer — these come
    // exclusively from render_styled() called by the new grid path.
    assert!(
        text.contains('█'),
        "Full-tier big-digit grid must render '█' block characters"
    );

    // The "World Clocks" separator header is the list path's signature.
    // The new big-digit grid renders individual Paragraph cells, not a
    // composite line with this header. Its absence proves the old
    // text-density list did not run.
    assert!(
        !text.contains("World Clocks"),
        "Full-tier big-digit grid must not render the list-path 'World Clocks' header"
    );
}

/// At Full size with 3 zones and max_scroll == 0, scroll keys must return
/// EventResult::Ignored (no scrolling needed; the grid shows everything).
#[test]
fn clock_full_tier_scroll_ignored_when_all_zones_fit() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    // After render max_scroll is 0; scroll keys must be Ignored.
    assert_eq!(
        widget.scroll_world_clocks(1),
        EventResult::Ignored,
        "scroll down must be Ignored when all zones fit"
    );
    assert_eq!(
        widget.scroll_world_clocks(-1),
        EventResult::Ignored,
        "scroll up must be Ignored when all zones fit"
    );
}

/// At Full size with many secondary zones, the grid must scroll when all
/// secondaries do not fit. `max_scroll > 0` means scrolling is available;
/// scroll down returns `Handled`, and after reaching max, scroll up walks back.
#[test]
fn clock_full_tier_grid_scrolls_through_secondaries() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: many_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    // First render to let the grid compute max_scroll.
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let max_scroll = widget.state.lock().unwrap().world_clock_max_scroll;
    assert!(
        max_scroll > 0,
        "15 secondary zones should overflow the Full grid: max_scroll must be > 0"
    );

    // Scroll down is Handled while max_scroll > 0 (existing scroll semantics).
    assert_eq!(
        widget.scroll_world_clocks(1),
        EventResult::Handled,
        "scroll down must be Handled when max_scroll > 0"
    );

    // Drive to max; then verify the offset is clamped there.
    for _ in 0..max_scroll {
        widget.scroll_world_clocks(1);
    }
    assert_eq!(
        widget.state.lock().unwrap().world_clock_scroll,
        max_scroll,
        "scroll must clamp at max_scroll"
    );
}

/// The secondary grid entries must contain only the configured secondary zones,
/// not the home/primary zone. Check this via the data method directly (the
/// home city name also appears in the title-bar timezone metadata, so
/// buffer-scanning would give a false positive).
#[test]
fn clock_full_tier_home_zone_excluded_from_grid() {
    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: vec![
            SecondaryTimezone {
                label: "Tokyo".into(),
                timezone: "Asia/Tokyo".into(),
            },
            SecondaryTimezone {
                label: "London".into(),
                timezone: "Europe/London".into(),
            },
        ],
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let entries = widget.world_clock_secondary_grid_entries();

    // Only the two configured secondaries should appear.
    assert_eq!(
        entries.len(),
        2,
        "secondary grid must contain exactly the 2 configured secondaries"
    );
    let labels: Vec<&str> = entries.iter().map(|(l, _, _, _)| l.as_str()).collect();
    assert!(
        labels.contains(&"Tokyo"),
        "secondary grid must include 'Tokyo'"
    );
    assert!(
        labels.contains(&"London"),
        "secondary grid must include 'London'"
    );
    // Home zone must not appear.
    assert!(
        !labels.contains(&"Vancouver"),
        "home zone 'Vancouver' must NOT be in the secondary grid entries"
    );
    assert!(
        !labels.contains(&"Local"),
        "'Local' must NOT appear in the secondary grid entries"
    );
}

/// At Full size each secondary zone cell must include a day + date string
/// (e.g. "Mon", "Tue", … combined with a month abbreviation and day number).
#[test]
fn clock_full_tier_grid_cells_show_day_and_date() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    // The rendered buffer must contain at least one weekday abbreviation
    // (from the date row of a grid cell). The month abbreviation must also
    // be present.
    let weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let has_weekday = weekdays.iter().any(|&d| text.contains(d));
    assert!(
        has_weekday,
        "Full-tier grid cells must render a weekday abbreviation in the date row"
    );

    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let has_month = months.iter().any(|&m| text.contains(m));
    assert!(
        has_month,
        "Full-tier grid cells must render a month abbreviation in the date row"
    );
}

/// At Full size with two secondary zones there should be a visual separator
/// between columns. The `│` character is used for inter-column separation
/// and must appear in the buffer when more than one column is rendered.
#[test]
fn clock_full_tier_grid_has_column_separators() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        // Two secondaries → two columns → one separator between them.
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains('│'),
        "Full-tier grid with multiple columns must render '│' separators between columns"
    );
}

/// At Full size, each clock's content (big digits, city label, date row) must be
/// horizontally centered within its column. We verify this by checking that the
/// separator `│` column's x is strictly between the widget's left border and the
/// right edge — meaning the columns are balanced across the width. Additionally,
/// with two secondaries at FULL_MIN_W the content must not be flush against the
/// left border (there is padding around the glyph block).
#[test]
fn clock_full_tier_grid_content_centered_with_breathing_room() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // Find at least one `│` separator in the buffer and assert it is not at
    // either extreme edge — it must sit somewhere in the middle third of the
    // available width, proving the columns are distributed across the full width
    // rather than crammed to one side.
    let mid_lo = area.x + area.width / 3;
    let mid_hi = area.x + 2 * area.width / 3;
    let sep_found = (area.y..area.bottom()).any(|y| {
        (mid_lo..mid_hi).any(|x| buf[(x, y)].symbol() == "│")
    });
    assert!(
        sep_found,
        "Full-tier grid separator '│' must appear in the middle third of the \
         buffer width, confirming columns span the full width"
    );

    // The big-digit block occupies 19 cols. The cell width at FULL_MIN_W with
    // 2 secondaries: inner = 103 cols, 2 cols → cell_w = 51. Content_w = 50
    // (separator in last col). Centered 19-col glyphs start at offset 15 within
    // the cell — well away from the left edge. Verify by checking that at least
    // one `█` glyph is not at x = 1 (border) or x = 2 (first content col).
    let first_block_x = (area.y..area.bottom())
        .flat_map(|y| (area.x..area.right()).map(move |x| (x, y)))
        .find(|&(x, y)| buf[(x, y)].symbol() == "█")
        .map(|(x, _)| x);

    if let Some(bx) = first_block_x {
        assert!(
            bx > area.x + 5,
            "big-digit '█' must not be flush against the left border — \
             centered content must have padding; found '█' at x={bx}"
        );
    }
}

/// At Full size, city labels appear in the buffer and a card-border separator
/// is present. At 160 cols with 2 secondaries: card_w=40, stride=41,
/// cols_per_row=3 capacity but only 2 zones → 2 cards rendered.
#[test]
fn clock_full_tier_grid_columns_span_full_width() {
    // Use a wider terminal to make the geometry easy to reason about.
    let (w, h) = (160u16, FULL_MIN_H);
    // 160×FULL_MIN_H is Full tier (160 >= FULL_MIN_W, h >= FULL_MIN_H).
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: 160×{h} must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let text = buffer_text(buf);

    // Both secondary city names must appear in the buffer (they appear as
    // centered labels inside their respective columns).
    assert!(
        text.contains("New York"),
        "Full-tier grid must render 'New York' label"
    );
    assert!(
        text.contains("London"),
        "Full-tier grid must render 'London' label"
    );

    // The separator must appear (2 columns → 1 separator).
    assert!(
        text.contains('│'),
        "Full-tier grid must render '│' inter-column separator"
    );
}

/// At Full size with 2 secondary zones in a very wide terminal, cards are
/// capped at 40 cols and the group is centered — not stretched to fill.
/// Geometry at 240 cols: inner ~238, card_w=40, stride=41, 2 cards,
/// group_w=81, outer_margin=78. Both cards appear in the centre; there
/// is blank space to the left and right of the group.
#[test]
fn clock_full_tier_two_zones_capped_and_centered_not_stretched() {
    // 240-col terminal: card_w=40, outer_margin=(238-81)/2=78.
    let (w, h) = (240u16, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: 240×{h} must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // Exclude the outermost widget border at y=0/h-1 and x=0/w-1 so we
    // don't accidentally match its full-width corner glyphs.
    let inner_x_start = area.x + 1;
    let inner_x_end = area.right() - 1;
    let inner_y_start = area.y + 1;
    let inner_y_end = area.bottom() - 1;

    // Cards are capped at 40 wide. Find card-corner glyphs strictly inside
    // the widget boundary to locate the leftmost and rightmost card edges.
    let leftmost_corner = (inner_y_start..inner_y_end)
        .flat_map(|y| (inner_x_start..inner_x_end).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            matches!(buf[(x, y)].symbol(), "╭" | "╰")
        })
        .map(|(x, _)| x)
        .min();

    let rightmost_corner = (inner_y_start..inner_y_end)
        .flat_map(|y| (inner_x_start..inner_x_end).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            matches!(buf[(x, y)].symbol(), "╮" | "╯")
        })
        .map(|(x, _)| x)
        .max();

    // The group is centered: there must be blank margin on both the left
    // and right sides of the card group. Geometry at 240 cols: inner=238,
    // card_w=40, 2 cards, group_w=81, outer_margin=78. Require at least
    // 10 cols of inset on each side.
    if let Some(lx) = leftmost_corner {
        assert!(
            lx > area.x + 10,
            "leftmost card corner must be inset from the left edge (centered group); \
             found corner at x={lx}, area.x={}", area.x
        );
    }
    if let Some(rx) = rightmost_corner {
        assert!(
            rx < area.right().saturating_sub(10),
            "rightmost card corner must be inset from the right edge (centered group, \
             not stretched); found corner at x={rx}, area.right()={}", area.right()
        );
    }

    // Each card is exactly 40 cols wide. Verify by measuring the distance
    // between the leftmost '╭' and the nearest '╮' on the same row inside
    // the widget boundary.
    let card_widths: Vec<u16> = (inner_y_start..inner_y_end)
        .filter_map(|y| {
            let left = (inner_x_start..inner_x_end).find(|&x| buf[(x, y)].symbol() == "╭")?;
            let right = (left..inner_x_end).find(|&x| buf[(x, y)].symbol() == "╮")?;
            Some(right - left + 1)
        })
        .collect();

    if !card_widths.is_empty() {
        for &cw in &card_widths {
            assert!(
                cw <= 40,
                "card width must not exceed 40 cols (CARD_MAX_W); measured {cw}"
            );
        }
    }
}

/// At Standard size (50 × 20) the scrollable list path runs. The "World
/// Clocks" header text is its signature. With 16 entries the list also
/// has max_scroll > 0, proving the Full grid did NOT activate.
#[test]
fn clock_standard_tier_shows_list_not_big_digit_grid() {
    let (w, h) = (50u16, 20u16);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Standard,
        "precondition: size must resolve to Standard"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        show_seconds_ticker: true,
        show_date: true,
        secondary_timezones: many_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    assert!(
        widget.state.lock().unwrap().world_clock_max_scroll > 0,
        "16 entries cannot all fit in a Standard cell: list must be scrollable"
    );

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains("World Clocks"),
        "Standard tier must render the 'World Clocks' list header (not the big-digit grid)"
    );
}

/// At Expanded size (EXPANDED_MIN_W × 24) the scrollable list path also
/// runs. Same assertions as Standard.
#[test]
fn clock_expanded_tier_shows_list_not_big_digit_grid() {
    let (w, h) = (EXPANDED_MIN_W, 24u16);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Expanded,
        "precondition: size must resolve to Expanded"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        show_seconds_ticker: true,
        show_date: true,
        secondary_timezones: many_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    assert!(
        widget.state.lock().unwrap().world_clock_max_scroll > 0,
        "16 entries cannot all fit in an Expanded cell: list must be scrollable"
    );

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains("World Clocks"),
        "Expanded tier must render the 'World Clocks' list header (not the big-digit grid)"
    );
}

// ── Full-tier card-border tests ───────────────────────────────────────

/// At Full size, each secondary zone cell must be rendered as a bordered card:
/// the rounded-corner border glyphs (`╭`, `╮`, `╰`, `╯`) must appear in the buffer.
/// The big-digit `█` characters must also be present (content is still rendered inside).
#[test]
fn clock_full_tier_grid_cells_have_card_borders() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());

    // Rounded card border corners.
    assert!(
        text.contains('╭') || text.contains('╮') || text.contains('╰') || text.contains('╯'),
        "Full-tier clock grid cells must render rounded card border corner glyphs"
    );

    // Content (big-digit blocks) is still inside the card.
    assert!(
        text.contains('█'),
        "Full-tier clock grid cells must render big-digit '█' characters inside the card"
    );
}

/// At Full size with card borders, big-digit content must be centered within the
/// card inner area: the `█` character must not appear at the leftmost column of
/// any cell (there should be horizontal padding between the card border and the glyph).
#[test]
fn clock_full_tier_card_content_centered_within_border() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // Find the leftmost `█` character. It must be at x > 3 (past the widget
    // outer border + card border + at least 1 col of inner padding).
    let first_block_x = (area.y..area.bottom())
        .flat_map(|y| (area.x..area.right()).map(move |x| (x, y)))
        .find(|&(x, y)| buf[(x, y)].symbol() == "█")
        .map(|(x, _)| x);

    if let Some(bx) = first_block_x {
        assert!(
            bx > area.x + 3,
            "big-digit '█' must not be flush against the left edge — \
             card border + inner pad must separate them; found '█' at x={bx}"
        );
    }
}

// ── Stopwatch mode ────────────────────────────────────────────────────

/// Build a widget already in Stopwatch mode with three laps recorded.
#[cfg(test)]
fn build_stopwatch_with_laps() -> ClockWidget {
    let w = build_widget(ClockConfig::default());
    {
        let mut st = w.state.lock().unwrap();
        st.mode = Mode::Stopwatch;
        st.stopwatch.laps = vec![
            Duration::from_millis(5_234),
            Duration::from_millis(10_891),
            Duration::from_millis(16_102),
        ];
    }
    w
}

/// At Full size, `render_stopwatch_lap_table` runs and writes the column
/// header " Lap  Split …" which is unique to the Full-tier lap table —
/// the compact list uses "Lap NN - …" format with no "Split" column label.
#[test]
fn stopwatch_full_tier_shows_lap_table() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let widget = build_stopwatch_with_laps();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains("Split"),
        "Full-tier lap table must render the 'Split' column header"
    );
}

/// At Standard size the compact lap list runs; it never renders a
/// "Split" column header so that text must be absent.
#[test]
fn stopwatch_standard_tier_no_lap_table_leaks() {
    let (w, h) = (50u16, 20u16);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Standard,
        "precondition: size must resolve to Standard"
    );

    let widget = build_stopwatch_with_laps();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        !text.contains("Split"),
        "Standard tier must not render the Full-tier 'Split' column header"
    );
}

/// At Expanded size the compact lap list also runs; same assertion.
#[test]
fn stopwatch_expanded_tier_no_lap_table_leaks() {
    let (w, h) = (EXPANDED_MIN_W, 24u16);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Expanded,
        "precondition: size must resolve to Expanded"
    );

    let widget = build_stopwatch_with_laps();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        !text.contains("Split"),
        "Expanded tier must not render the Full-tier 'Split' column header"
    );
}

// ── Timer mode ────────────────────────────────────────────────────────

/// Build a widget in Timer mode, Paused with 3 minutes remaining of a
/// 5-minute duration.  Elapsed fraction = 40 %.
#[cfg(test)]
fn build_paused_timer() -> ClockWidget {
    let w = build_widget(ClockConfig::default());
    {
        let mut st = w.state.lock().unwrap();
        st.mode = Mode::Timer;
        st.timer.duration = Duration::from_secs(300);
        st.timer.phase = TimerPhase::Paused {
            remaining: Duration::from_secs(180),
        };
    }
    w
}

/// At Full size, the burn-down bar detail row contains "remaining" — a
/// word that does not appear in the Standard/Expanded timer hints.
#[test]
fn timer_full_tier_shows_burndown_bar() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let widget = build_paused_timer();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        text.contains("remaining"),
        "Full-tier burn-down bar must render a detail row containing 'remaining'"
    );
}

/// At Standard size the burn-down bar is not rendered; none of the
/// Standard-tier timer hints contain the word "remaining".
#[test]
fn timer_standard_tier_no_burndown_bar_leaks() {
    let (w, h) = (50u16, 20u16);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Standard,
        "precondition: size must resolve to Standard"
    );

    let widget = build_paused_timer();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        !text.contains("remaining"),
        "Standard tier must not render the Full-tier burn-down bar detail row"
    );
}

/// At Expanded size the burn-down bar is also absent.
#[test]
fn timer_expanded_tier_no_burndown_bar_leaks() {
    let (w, h) = (EXPANDED_MIN_W, 24u16);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Expanded,
        "precondition: size must resolve to Expanded"
    );

    let widget = build_paused_timer();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let text = buffer_text(terminal.backend().buffer());
    assert!(
        !text.contains("remaining"),
        "Expanded tier must not render the Full-tier burn-down bar detail row"
    );
}

// ── Full-tier card-width cap and centering tests ───────────────────────

/// Each timezone card in the Full-tier grid must be at most 40 columns wide.
/// This is verified by measuring the distance between '╭' and the nearest '╮'
/// on every row inside the grid area (rows after the top face + separator row).
/// The outermost widget border is excluded — its corners span the full width.
#[test]
fn clock_full_tier_card_width_capped_at_40() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // Skip the outermost widget border rows (y=0 top border, y=h-1 bottom border)
    // and the outermost border columns (x=0 left, x=w-1 right). Card corners
    // must appear strictly inside the widget boundary.
    let inner_x_start = area.x + 1;
    let inner_x_end = area.right() - 1;
    let inner_y_start = area.y + 1;
    let inner_y_end = area.bottom() - 1;

    let mut card_found = false;
    for y in inner_y_start..inner_y_end {
        let left = (inner_x_start..inner_x_end).find(|&x| buf[(x, y)].symbol() == "╭");
        if let Some(lx) = left {
            let right = (lx..inner_x_end).find(|&x| buf[(x, y)].symbol() == "╮");
            if let Some(rx) = right {
                let cw = rx - lx + 1;
                assert!(
                    cw <= 40,
                    "card width must be ≤40 (CARD_MAX_W); measured {cw} on row y={y}"
                );
                card_found = true;
            }
        }
    }
    assert!(card_found, "at least one card border row (╭…╮) must be present inside the widget");
}

/// With few zones in a wide pane (200 cols), cards stay at 40 wide and
/// there is outer margin on each side — the group is not stretched to fill.
/// This directly verifies the non-stretch property: leftmost card corner
/// must be meaningfully inset from the widget's left inner edge.
///
/// Geometry: inner area = 198 cols (200 - 2 outer borders). 1 secondary →
/// 1 card, group_w=40, outer_margin=(198-40)/2=79. The card '╭' corner
/// appears at inner_x=1 + outer_margin=79 → absolute x≈80.
#[test]
fn clock_full_tier_wide_pane_few_zones_has_outer_margin() {
    let (w, h) = (200u16, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: 200×{h} must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        // 1 secondary: group_w=40, inner=198, outer_margin=79
        secondary_timezones: vec![super::config::SecondaryTimezone {
            label: "Tokyo".into(),
            timezone: "Asia/Tokyo".into(),
        }],
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // Exclude the outermost widget border at y=0/h-1 and x=0/w-1 so we
    // don't accidentally match its corner glyphs.
    let inner_x_start = area.x + 1;
    let inner_x_end = area.right() - 1;
    let inner_y_start = area.y + 1;
    let inner_y_end = area.bottom() - 1;

    // The leftmost '╭' corner inside the widget must be well inset from the
    // widget's left inner column (x=1). With outer_margin≈79, require ≥20.
    let leftmost_corner = (inner_y_start..inner_y_end)
        .flat_map(|y| (inner_x_start..inner_x_end).map(move |x| (x, y)))
        .filter(|&(x, y)| buf[(x, y)].symbol() == "╭")
        .map(|(x, _)| x)
        .min();

    if let Some(lx) = leftmost_corner {
        assert!(
            lx > area.x + 20,
            "card must be centered with outer margin; leftmost '╭' at x={lx} \
             is too close to the left edge (area.x={})", area.x
        );
    } else {
        panic!("no card corner '╭' found inside the widget — card was not rendered");
    }

    // Symmetric: rightmost '╮' must also be inset from the right edge.
    let rightmost_corner = (inner_y_start..inner_y_end)
        .flat_map(|y| (inner_x_start..inner_x_end).map(move |x| (x, y)))
        .filter(|&(x, y)| buf[(x, y)].symbol() == "╮")
        .map(|(x, _)| x)
        .max();

    if let Some(rx) = rightmost_corner {
        assert!(
            rx < area.right().saturating_sub(20),
            "card must be centered with right outer margin; rightmost '╮' at x={rx} \
             is too close to the right edge (area.right()={})", area.right()
        );
    }
}

/// The inter-card gap between adjacent cards is exactly 1 column.
/// With 2 secondary zones, the right edge of card 0 (the '╮') and the
/// left edge of card 1 (the '╭') must be exactly 2 columns apart
/// (gap = card1_left - card0_right - 1 = 1).
#[test]
fn clock_full_tier_inter_card_gap_is_one_col() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: few_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let buf = terminal.backend().buffer();
    let area = buf.area;

    // Exclude the outermost widget border at y=0/h-1 and x=0/w-1.
    let inner_x_start = area.x + 1;
    let inner_x_end = area.right() - 1;
    let inner_y_start = area.y + 1;
    let inner_y_end = area.bottom() - 1;

    // On each row find any '╮' that has a '╭' somewhere to its right inside
    // the widget boundary. The gap = left_of_card2 - right_of_card1 - 1 must be 1.
    let mut gap_found = false;
    for y in inner_y_start..inner_y_end {
        // Collect all '╮' and '╭' positions on this row (inner boundary only).
        let rights: Vec<u16> = (inner_x_start..inner_x_end)
            .filter(|&x| buf[(x, y)].symbol() == "╮")
            .collect();
        let lefts: Vec<u16> = (inner_x_start..inner_x_end)
            .filter(|&x| buf[(x, y)].symbol() == "╭")
            .collect();
        // Match each '╮' with the next '╭' that follows it.
        for &r in &rights {
            if let Some(&l) = lefts.iter().find(|&&l| l > r) {
                let gap = l - r - 1;
                assert!(
                    gap == 1,
                    "inter-card gap must be exactly 1 col; got {gap} on row y={y} \
                     (╮ at x={r}, ╭ at x={l})"
                );
                gap_found = true;
            }
        }
    }
    assert!(
        gap_found,
        "no adjacent card pair (╮ followed by ╭) found inside the widget — need at least 2 cards"
    );
}

/// Scroll/overflow capacity is recomputed from the fixed-width layout.
/// With CARD_MAX_W=40 at FULL_MIN_W (inner~103): cols_per_row=2,
/// row_count from height, capacity = cols_per_row * row_count.
/// With 15 secondaries that exceed this capacity, max_scroll > 0.
/// Scrolling still works (Handled when max_scroll > 0).
#[test]
fn clock_full_tier_capacity_and_scroll_with_fixed_width_layout() {
    let (w, h) = (FULL_MIN_W, FULL_MIN_H);
    assert_eq!(
        ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
        ViewTier::Full,
        "precondition: size must resolve to Full"
    );

    let cfg = ClockConfig {
        timezone: Some("America/Vancouver".into()),
        secondary_timezones: many_secondaries(),
        ..ClockConfig::default()
    };
    let widget = build_widget(cfg);

    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();

    let max_scroll = widget.state.lock().unwrap().world_clock_max_scroll;
    assert!(
        max_scroll > 0,
        "15 secondaries must overflow fixed-width capacity; max_scroll={max_scroll}"
    );

    // Scrolling is functional.
    assert_eq!(
        widget.scroll_world_clocks(1),
        EventResult::Handled,
        "scroll down must be Handled when max_scroll > 0"
    );
    let offset_after = widget.state.lock().unwrap().world_clock_scroll;
    assert_eq!(offset_after, 1, "scroll offset must advance to 1");

    // Re-render at new offset: the widget must still render without panic
    // and the buffer must still contain big-digit blocks.
    let backend2 = TestBackend::new(w, h);
    let mut terminal2 = Terminal::new(backend2).unwrap();
    terminal2
        .draw(|frame| widget.render(frame, frame.area(), false))
        .unwrap();
    let text = buffer_text(terminal2.backend().buffer());
    assert!(
        text.contains('█'),
        "big-digit blocks must still render after scrolling"
    );
}

/// Standard and Expanded tiers are completely unaffected by the Full-tier
/// card-cap change. Stopwatch and timer modes are unaffected too.
/// This test renders all three clock modes at Standard and Expanded to
/// verify no regressions in those paths.
#[test]
fn clock_non_full_tiers_unaffected_by_card_cap() {
    use super::state::Mode;
    use super::timer::TimerPhase;
    use std::time::Duration;

    let sizes: &[(u16, u16, ViewTier)] = &[
        (50, 20, ViewTier::Standard),
        (EXPANDED_MIN_W, 24, ViewTier::Expanded),
    ];

    for &(w, h, expected_tier) in sizes {
        assert_eq!(
            ViewTier::from_rect(ratatui::layout::Rect::new(0, 0, w, h)),
            expected_tier,
            "precondition: {w}×{h} must resolve to {expected_tier:?}"
        );

        // Clock mode.
        let cfg = ClockConfig {
            timezone: Some("America/Vancouver".into()),
            secondary_timezones: many_secondaries(),
            ..ClockConfig::default()
        };
        let clock_widget = build_widget(cfg);
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| clock_widget.render(frame, frame.area(), false))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("World Clocks"),
            "{expected_tier:?} clock mode must render 'World Clocks' list header at {w}×{h}"
        );

        // Stopwatch mode with laps.
        let sw_widget = build_stopwatch_with_laps();
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| sw_widget.render(frame, frame.area(), false))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            !text.contains("Split"),
            "{expected_tier:?} stopwatch must not show Full-tier 'Split' header at {w}×{h}"
        );

        // Timer mode (paused).
        let timer_widget = {
            let w2 = build_widget(ClockConfig::default());
            {
                let mut st = w2.state.lock().unwrap();
                st.mode = Mode::Timer;
                st.timer.duration = Duration::from_secs(300);
                st.timer.phase = TimerPhase::Paused {
                    remaining: Duration::from_secs(180),
                };
            }
            w2
        };
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| timer_widget.render(frame, frame.area(), false))
            .unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            !text.contains("remaining"),
            "{expected_tier:?} timer must not show Full-tier burn-down detail at {w}×{h}"
        );
    }
}
