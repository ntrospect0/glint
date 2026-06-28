// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Unit tests for the calendar widget. Split out of `mod.rs` to keep the
//! widget-entry file readable; everything else is unchanged.

use super::*;
use std::collections::HashMap;
use chrono::TimeZone;
use crate::theme::parse_color;
use super::nav::{
    advance_month, bottom_action_at, content_rect_for, first_of_next_month, google_calendar_url,
    local_midnight, month_long, outlook_calendar_url, rotated_weekday_labels, start_of_month,
    start_of_week, weekday_short, BottomAction, WebTarget,
};
use super::colors::CalendarColors;

    fn build_widget(cfg: CalendarConfig) -> CalendarWidget {
        CalendarWidget::with_config(
            "main".to_string(),
            cfg,
            Arc::new(Theme::builtin_defaults()),
            ScopedCache::ephemeral(),
        )
    }

    #[test]
    fn google_url_carries_view_and_anchor_date() {
        let date = NaiveDate::from_ymd_opt(2026, 5, 28).unwrap();
        assert_eq!(
            google_calendar_url(CalendarView::Day, date),
            "https://calendar.google.com/calendar/u/0/r/day/2026/5/28"
        );
        assert_eq!(
            google_calendar_url(CalendarView::Week, date),
            "https://calendar.google.com/calendar/u/0/r/week/2026/5/28"
        );
        assert_eq!(
            google_calendar_url(CalendarView::Month, date),
            "https://calendar.google.com/calendar/u/0/r/month/2026/5/28"
        );
    }

    #[test]
    fn google_url_does_not_zero_pad_month_or_day() {
        // Google's `/r/{view}/{Y}/{M}/{D}` deep-link expects unpadded
        // integers; padding turns the route into a 404. Lock that in.
        let date = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();
        assert_eq!(
            google_calendar_url(CalendarView::Day, date),
            "https://calendar.google.com/calendar/u/0/r/day/2026/1/5"
        );
    }

    #[test]
    fn outlook_url_picks_per_view_path() {
        // Lowercase segments on the `outlook.cloud.microsoft` surface —
        // verified working against M365 May 2026. An earlier draft used
        // capitalized segments on `outlook.office.com` which silently
        // redirected to the user's saved default view instead of the
        // requested one.
        assert_eq!(
            outlook_calendar_url(CalendarView::Day),
            "https://outlook.cloud.microsoft/calendar/view/day"
        );
        assert_eq!(
            outlook_calendar_url(CalendarView::Week),
            "https://outlook.cloud.microsoft/calendar/view/week"
        );
        assert_eq!(
            outlook_calendar_url(CalendarView::Month),
            "https://outlook.cloud.microsoft/calendar/view/month"
        );
    }

    fn mouse_scroll(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// One horizontal-scroll click steps the anchor by the view's
    /// `nav_step` (Day → 1, Week → 7, Month → 30). Cooldown is cleared
    /// between calls so the test exercises the per-event stride, not
    /// the debounce gate.
    #[test]
    fn horizontal_scroll_steps_anchor_by_view_stride() {
        for (view, days) in [
            (CalendarView::Day, 1),
            (CalendarView::Week, 7),
            (CalendarView::Month, 30),
        ] {
            let mut w = build_widget(CalendarConfig::default());
            w.view = view;
            let start = w.anchor;
            let area = Rect::new(0, 0, 40, 20);
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollRight), area);
            assert_eq!(
                w.anchor,
                start + ChronoDuration::days(days),
                "view {view:?}: ScrollRight should advance by {days} day(s)"
            );
            // Clear the cooldown so the next click isn't dropped by the
            // burst debounce — we're verifying per-event stride here.
            w.last_horizontal_scroll = None;
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollLeft), area);
            assert_eq!(w.anchor, start, "view {view:?}: ScrollLeft should reverse");
        }
    }

    /// A burst of ScrollRight events arriving within the cooldown
    /// collapses to one navigation step. Without this, a trackpad flick
    /// (20-30 events in ~300ms) jumps 20+ days at once.
    #[test]
    fn horizontal_scroll_burst_within_cooldown_collapses_to_one_step() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Day;
        let start = w.anchor;
        let area = Rect::new(0, 0, 40, 20);
        for _ in 0..20 {
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollRight), area);
        }
        assert_eq!(
            w.anchor,
            start + ChronoDuration::days(1),
            "rapid burst within cooldown should advance only once"
        );
    }

    /// macOS trackpads emit micro horizontal-scroll events interspersed
    /// with vertical ones. Without axis-lock, the horizontal jitter would
    /// fire date navigation in the middle of agenda scrolling and undo
    /// every row of vertical motion. After a vertical scroll, any
    /// horizontal scroll within the lock window must be dropped.
    #[test]
    fn vertical_scroll_locks_out_horizontal_jitter() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Day;
        let start = w.anchor;
        let area = Rect::new(0, 0, 40, 20);
        w.handle_mouse(mouse_scroll(MouseEventKind::ScrollDown), area);
        // Clearing the horizontal cooldown isolates the test: if the
        // horizontal event gets through, it's the axis-lock that broke,
        // not the burst debounce.
        w.last_horizontal_scroll = None;
        w.handle_mouse(mouse_scroll(MouseEventKind::ScrollLeft), area);
        w.handle_mouse(mouse_scroll(MouseEventKind::ScrollRight), area);
        assert_eq!(
            w.anchor, start,
            "horizontal jitter during a vertical gesture must not navigate"
        );
        // Simulate the lock expiring (user paused after vertical) — a
        // deliberate horizontal flick should now navigate.
        w.last_vertical_scroll = None;
        w.handle_mouse(mouse_scroll(MouseEventKind::ScrollRight), area);
        assert_eq!(w.anchor, start + ChronoDuration::days(1));
    }

    /// In Week view, scrolling over a specific day-column drives that
    /// column's offset only — neighbours stay put. Catches a regression
    /// where one shared scroll state would shift every column together
    /// (or where Week view dropped wheel events entirely).
    #[test]
    fn week_view_wheel_scrolls_targeted_column_only() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Week;
        // 70 cols wide → ~10 cols per day, so column index 0 sits at
        // x ∈ [1, 10] (after the 1-col border inset). Target column 2
        // (Tuesday) at x = 22.
        let area = Rect::new(0, 0, 70, 20);
        // Pre-seed scroll_max so the clamp lets the offset move.
        // Render normally writes this; in the test we set it directly.
        {
            let mut st = w.state.lock().unwrap();
            st.week_col_scroll_max = [10; 7];
        }
        let mut evt = mouse_scroll(MouseEventKind::ScrollDown);
        evt.column = 22;
        w.handle_mouse(evt, area);
        let scrolls = w.state.lock().unwrap().week_col_scroll;
        let nonzero_count = scrolls.iter().filter(|&&v| v > 0).count();
        assert_eq!(
            nonzero_count, 1,
            "exactly one column should scroll; got {scrolls:?}"
        );
        assert!(
            scrolls[2] > 0,
            "the Tuesday column (index 2) should be the one scrolled; got {scrolls:?}"
        );
    }

    /// Vertical scroll never moves the anchor — even in Week view where
    /// the wheel routes through a different helper.
    #[test]
    fn vertical_scroll_does_not_move_anchor() {
        for view in [CalendarView::Day, CalendarView::Week, CalendarView::Month] {
            let mut w = build_widget(CalendarConfig::default());
            w.view = view;
            let start = w.anchor;
            let area = Rect::new(0, 0, 40, 20);
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollUp), area);
            w.handle_mouse(mouse_scroll(MouseEventKind::ScrollDown), area);
            assert_eq!(
                w.anchor, start,
                "view {view:?}: vertical scroll moved anchor"
            );
        }
    }

    /// Day view: [Today] is lit only when the anchor IS today.
    #[test]
    fn today_button_state_in_day_view_tracks_anchor() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Day;
        w.anchor = Local::now().date_naive();
        assert!(w.current_view_contains_today());
        w.anchor -= ChronoDuration::days(3);
        assert!(!w.current_view_contains_today());
    }

    /// Week view: today is "in view" when it falls inside the Sun..=Sat
    /// window containing the anchor — not just when the anchor itself
    /// is today. Walking 3 days forward or back from today stays in
    /// the same week (most of the time).
    #[test]
    fn today_button_state_in_week_view_covers_whole_week() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Week;
        let today = Local::now().date_naive();
        // Anchor on the start of the current week → should still
        // count as "today in view."
        w.anchor = start_of_week(today, Weekday::Sun);
        assert!(w.current_view_contains_today());
        // Jump to the start of a different week — today is no longer
        // inside the anchored Sun..=Sat range.
        w.anchor = start_of_week(today, Weekday::Sun) - ChronoDuration::days(14);
        assert!(!w.current_view_contains_today());
    }

    /// Month view: any day within today's calendar month counts.
    /// Crossing the month boundary flips the state.
    #[test]
    fn today_button_state_in_month_view_covers_whole_month() {
        let mut w = build_widget(CalendarConfig::default());
        w.view = CalendarView::Month;
        let today = Local::now().date_naive();
        // Anchor on the 1st of this month — same month → lit.
        w.anchor = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
        assert!(w.current_view_contains_today());
        // Anchor on the previous month's 15th.
        w.anchor = first_of_next_month(today) + ChronoDuration::days(45);
        assert!(!w.current_view_contains_today());
    }

    #[test]
    fn start_of_week_anchors_on_configured_first_day() {
        // 2026-05-20 is a Wednesday.
        let wed = NaiveDate::from_ymd_opt(2026, 5, 20).unwrap();
        let sun = start_of_week(wed, Weekday::Sun);
        assert_eq!(sun.weekday(), Weekday::Sun);
        assert_eq!(sun, NaiveDate::from_ymd_opt(2026, 5, 17).unwrap());
        // ISO/Europe default — Monday anchors one day later.
        let mon = start_of_week(wed, Weekday::Mon);
        assert_eq!(mon.weekday(), Weekday::Mon);
        assert_eq!(mon, NaiveDate::from_ymd_opt(2026, 5, 18).unwrap());
        // A weekday that's strictly *after* today rolls back through
        // the prior week, not forward — Saturday-start asked on a
        // Wednesday lands on the previous Saturday (5 days back).
        let sat = start_of_week(wed, Weekday::Sat);
        assert_eq!(sat.weekday(), Weekday::Sat);
        assert_eq!(sat, NaiveDate::from_ymd_opt(2026, 5, 16).unwrap());
    }

    #[test]
    fn rotated_weekday_labels_match_first_day() {
        assert_eq!(
            rotated_weekday_labels(Weekday::Sun),
            ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
        );
        assert_eq!(
            rotated_weekday_labels(Weekday::Mon),
            ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        );
        assert_eq!(
            rotated_weekday_labels(Weekday::Sat),
            ["Sat", "Sun", "Mon", "Tue", "Wed", "Thu", "Fri"]
        );
    }

    #[test]
    fn first_of_next_month_wraps_december() {
        let dec = NaiveDate::from_ymd_opt(2026, 12, 15).unwrap();
        let jan = first_of_next_month(dec);
        assert_eq!(jan, NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
    }

    #[test]
    fn color_resolver_is_stable_and_disambiguates_sources() {
        let cfg = CalendarConfig {
            providers: vec![
                ProviderEntry {
                    kind: ProviderKind::Google,
                    calendar_ids: vec!["primary".into()],
                },
                ProviderEntry {
                    kind: ProviderKind::Outlook,
                    calendar_ids: vec!["primary".into()],
                },
            ],
            ..Default::default()
        };
        let c = CalendarColors::build(&cfg);
        let g = c.resolve("google", "primary");
        let o = c.resolve("outlook", "primary");
        assert_ne!(g, o, "same calendar id under different sources must differ");
        assert_eq!(g, c.resolve("google", "primary"), "must be deterministic");
    }

    #[test]
    fn explicit_calendar_color_overrides_sequence() {
        let mut overrides = HashMap::new();
        overrides.insert("google:primary".to_string(), "red".to_string());
        let cfg = CalendarConfig {
            providers: vec![ProviderEntry {
                kind: ProviderKind::Google,
                calendar_ids: vec!["primary".into()],
            }],
            calendar_colors: overrides,
            ..Default::default()
        };
        let c = CalendarColors::build(&cfg);
        assert_eq!(c.resolve("google", "primary"), Color::Red);
    }

    #[test]
    fn custom_palette_replaces_default_sequence() {
        let cfg = CalendarConfig {
            providers: vec![ProviderEntry {
                kind: ProviderKind::Google,
                calendar_ids: vec!["a".into(), "b".into()],
            }],
            color_palette: vec!["red".into(), "green".into()],
            ..Default::default()
        };
        let c = CalendarColors::build(&cfg);
        assert_eq!(c.resolve("google", "a"), Color::Red);
        assert_eq!(c.resolve("google", "b"), Color::Green);
    }

    #[test]
    fn parse_color_accepts_common_names_and_hex() {
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("Light-Blue"), Some(Color::LightBlue));
        assert_eq!(parse_color("BRIGHT_GREEN"), Some(Color::LightGreen));
        // Theme parser distinguishes "gray" (bright) from "dark_gray"
        // (the darker variant) — the calendar parser used to fold both
        // into DarkGray; the shared parser treats them as separate
        // ANSI slots, matching ratatui's enum.
        assert_eq!(parse_color(" gray "), Some(Color::Gray));
        assert_eq!(parse_color("dark_gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("#ff6480"), Some(Color::Rgb(0xff, 0x64, 0x80)));
        assert_eq!(parse_color("#4097E4"), Some(Color::Rgb(0x40, 0x97, 0xe4)));
        assert_eq!(parse_color("nope"), None);
    }

    #[test]
    fn default_view_is_day_and_widget_starts_today() {
        let w = build_widget(CalendarConfig::default());
        assert_eq!(w.view, CalendarView::Day);
        assert_eq!(w.anchor, Local::now().date_naive());
    }

    #[test]
    fn bottom_action_at_maps_cols_to_actions() {
        // Bottom row renders: " [Today] [Day] [Week] [Month]"
        //                       1     7 9   13 15   20 22
        assert_eq!(bottom_action_at(2, 0), Some(BottomAction::Today));
        assert_eq!(bottom_action_at(7, 0), Some(BottomAction::Today)); // ']' position
        assert_eq!(
            bottom_action_at(10, 0),
            Some(BottomAction::View(CalendarView::Day))
        );
        assert_eq!(
            bottom_action_at(16, 0),
            Some(BottomAction::View(CalendarView::Week))
        );
        assert_eq!(
            bottom_action_at(23, 0),
            Some(BottomAction::View(CalendarView::Month))
        );
        assert_eq!(bottom_action_at(60, 0), None);
    }

    #[test]
    fn week_day_at_maps_columns_to_dates() {
        // Anchor on a Wednesday; weeks start Sunday.
        let cfg = CalendarConfig {
            default_view: CalendarView::Week,
            ..CalendarConfig::default()
        };
        let mut w = build_widget(cfg);
        w.anchor = NaiveDate::from_ymd_opt(2026, 5, 20).unwrap();
        let inner = Rect::new(0, 0, 70, 20);
        // 70 wide → each of the 7 cols ≈ 10. Click in col 0 (x=2) → Sunday.
        assert_eq!(
            w.week_day_at(2, 1, inner),
            Some(NaiveDate::from_ymd_opt(2026, 5, 17).unwrap())
        );
        // Click in column for Wednesday (col 3, x≈30+).
        assert_eq!(
            w.week_day_at(32, 5, inner),
            Some(NaiveDate::from_ymd_opt(2026, 5, 20).unwrap())
        );
        // Click in the hint row → None.
        assert_eq!(w.week_day_at(2, 19, inner), None);
    }

    #[test]
    fn month_day_at_maps_grid_cells_to_dates() {
        let cfg = CalendarConfig {
            default_view: CalendarView::Month,
            ..CalendarConfig::default()
        };
        let mut w = build_widget(cfg);
        w.anchor = NaiveDate::from_ymd_opt(2026, 5, 20).unwrap();
        // 40-wide column → 35-char grid centered → 2 cols leading padding,
        // so cell 0 starts at col 2, cell 6 starts at col 32.
        let inner = Rect::new(0, 0, 40, 20);
        // Rows: padding=0, month name=1, weekday header=2, weeks start at 3.
        // May 2026 starts Friday → first grid row is Sun Apr 26 … Sat May 2.
        let apr26 = NaiveDate::from_ymd_opt(2026, 4, 26).unwrap();
        assert_eq!(w.month_day_at(3, 3, inner), Some(apr26));
        let may2 = NaiveDate::from_ymd_opt(2026, 5, 2).unwrap();
        assert_eq!(w.month_day_at(33, 3, inner), Some(may2));
        // Clicks in padding / month-name / weekday-header rows → None.
        assert_eq!(w.month_day_at(3, 0, inner), None);
        assert_eq!(w.month_day_at(3, 1, inner), None);
        assert_eq!(w.month_day_at(3, 2, inner), None);
        // Beyond the 7th column of the grid → None.
        assert_eq!(w.month_day_at(38, 3, inner), None);
    }

    #[test]
    fn advance_month_wraps_year_boundaries() {
        assert_eq!(advance_month(2026, 12, 1), (2027, 1));
        assert_eq!(advance_month(2026, 1, -1), (2025, 12));
        assert_eq!(advance_month(2026, 5, 0), (2026, 5));
        assert_eq!(advance_month(2026, 5, 7), (2026, 12));
        assert_eq!(advance_month(2026, 5, 8), (2027, 1));
    }

    #[test]
    fn wrap_event_title_caps_lines_with_ellipsis() {
        let lines = wrap_event_title("the quick brown fox jumps over the lazy dog", 7, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines.last().unwrap().ends_with('…'));
    }

    #[test]
    fn wrap_event_title_fills_to_column_width() {
        // Char-level wrap: every line except the last (or one short of
        // the truncation point) should hit max_width exactly, so we
        // use every available column instead of leaving trailing
        // whitespace from a word-boundary-only wrap.
        let lines = wrap_event_title("Project planning meeting with vendor", 10, 4);
        for line in lines.iter().take(lines.len() - 1) {
            assert_eq!(
                line.chars().count(),
                10,
                "non-final line should fill the column: {line:?}"
            );
        }
    }

    #[test]
    fn wrap_event_title_splits_oversized_word_across_lines() {
        // A single 20-char word at column width 5: should occupy 4
        // lines of 5 chars each — no characters dropped, no
        // mid-string ellipsis.
        let lines = wrap_event_title("supercalifragilistic", 5, 4);
        assert_eq!(lines, vec!["super", "calif", "ragil", "istic"]);
    }

    #[test]
    fn wrap_event_title_ellipsises_truncated_oversized_word() {
        // Same word, only 3 lines available: the first 15 chars land
        // intact across lines 1+2, the last line keeps 4 chars + the
        // ellipsis (replacing the would-be 5th char).
        let lines = wrap_event_title("supercalifragilistic", 5, 3);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].ends_with('…'));
        assert_eq!(lines[2].chars().count(), 5);
    }

    #[test]
    fn wrap_event_title_skips_leading_space_on_continuation() {
        // When the break lands right before a space, the continuation
        // line shouldn't begin with that space — the user would see
        // an awkward indent.
        let lines = wrap_event_title("Hello World", 5, 3);
        assert_eq!(lines[0], "Hello");
        assert_eq!(lines[1], "World");
    }

    fn make_event(
        start: chrono::DateTime<Local>,
        end: chrono::DateTime<Local>,
        title: &str,
    ) -> Event {
        Event {
            title: title.into(),
            start,
            end,
            all_day: false,
            source: "local".into(),
            calendar: "test".into(),
            location: None,
        }
    }

    #[test]
    fn first_future_event_line_skips_past_events() {
        let w = build_widget(CalendarConfig::default());
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 21, 14, 0, 0)
            .unwrap();
        let one_hour = chrono::Duration::hours(1);
        // Three events: 09–10 (past), 12–13 (past), 15–16 (future).
        let events: Vec<Event> = vec![
            make_event(
                now - chrono::Duration::hours(5),
                now - chrono::Duration::hours(4),
                "morning standup",
            ),
            make_event(
                now - chrono::Duration::hours(2),
                now - chrono::Duration::hours(1),
                "lunch chat",
            ),
            make_event(now + one_hour, now + one_hour * 2, "design review"),
        ];
        let refs: Vec<&Event> = events.iter().collect();
        // Each event with no location and a short title takes exactly 1 line.
        // So the third event lands at line 2.
        let line = w.first_future_event_line(&refs, 60, now);
        assert_eq!(line, Some(2));
    }

    #[test]
    fn first_future_event_line_returns_none_when_all_events_past() {
        let w = build_widget(CalendarConfig::default());
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 21, 23, 0, 0)
            .unwrap();
        let events = vec![make_event(
            now - chrono::Duration::hours(10),
            now - chrono::Duration::hours(9),
            "long-finished meeting",
        )];
        let refs: Vec<&Event> = events.iter().collect();
        assert_eq!(w.first_future_event_line(&refs, 60, now), None);
    }

    #[test]
    fn first_future_event_line_includes_in_progress_event() {
        let w = build_widget(CalendarConfig::default());
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 21, 14, 30, 0)
            .unwrap();
        // Event is 14:00–15:00 — currently in progress; should qualify.
        let events = vec![make_event(
            now - chrono::Duration::minutes(30),
            now + chrono::Duration::minutes(30),
            "in-progress sync",
        )];
        let refs: Vec<&Event> = events.iter().collect();
        assert_eq!(w.first_future_event_line(&refs, 60, now), Some(0));
    }

    /// The pure rollover-gating helper: no new day → no roll; an
    /// unfocused (or idle-focused) widget is due; a focused, recently-
    /// active widget defers.
    #[test]
    fn auto_roll_due_gates_on_focus_and_idle() {
        use super::state::auto_roll_due;
        let day0 = NaiveDate::from_ymd_opt(2026, 6, 22).unwrap();
        let day1 = NaiveDate::from_ymd_opt(2026, 6, 23).unwrap();
        let day3 = NaiveDate::from_ymd_opt(2026, 6, 25).unwrap();
        let active = std::time::Duration::from_secs(10);
        let idle = AUTO_ROLL_FOCUSED_IDLE + std::time::Duration::from_secs(1);

        // Same day → never due, regardless of focus/idle.
        assert!(!auto_roll_due(day0, day0, false, idle));
        assert!(!auto_roll_due(day0, day0, true, active));
        // Clock ran backward (TZ / NTP) → not due; caller resyncs.
        assert!(!auto_roll_due(day0, day1, false, idle));

        // Unfocused → due immediately, however many days elapsed.
        assert!(auto_roll_due(day1, day0, false, active));
        assert!(auto_roll_due(day3, day0, false, active));

        // Focused: defer while active, allow once idle past the grace.
        assert!(!auto_roll_due(day1, day0, true, active));
        assert!(auto_roll_due(day1, day0, true, idle));
    }

    /// An unfocused widget left showing "today" snaps to the real today
    /// when `maybe_auto_roll` runs after the date has advanced — even
    /// across several midnights (machine slept over a weekend).
    #[test]
    fn unfocused_widget_snaps_to_today() {
        let mut w = build_widget(CalendarConfig::default());
        let today = Local::now().date_naive();
        w.anchor = today - ChronoDuration::days(2);
        w.rollover_date = today - ChronoDuration::days(2);
        w.is_focused
            .store(false, std::sync::atomic::Ordering::Relaxed);
        w.maybe_auto_roll();
        assert_eq!(w.anchor, today, "should snap to the real today");
        assert_eq!(w.rollover_date, today, "baseline advances with the anchor");
    }

    /// A view parked on a future date snaps home to today on rollover
    /// (not advanced by one) so an unattended dashboard returns to the
    /// live day.
    #[test]
    fn auto_roll_snaps_a_future_view_to_today() {
        let mut w = build_widget(CalendarConfig::default());
        let today = Local::now().date_naive();
        // As of "yesterday" the user had parked the view 5 days ahead.
        w.anchor = today + ChronoDuration::days(5);
        w.rollover_date = today - ChronoDuration::days(1);
        w.is_focused
            .store(false, std::sync::atomic::Ordering::Relaxed);
        w.maybe_auto_roll();
        assert_eq!(w.anchor, today, "future view should snap home to today");
    }

    /// A view navigated into the past also snaps home to today when the
    /// day rolls over unattended — every stale view returns to the live
    /// day, not just today-or-future ones.
    #[test]
    fn auto_roll_snaps_a_past_view_to_today() {
        let mut w = build_widget(CalendarConfig::default());
        let today = Local::now().date_naive();
        w.anchor = today - ChronoDuration::days(10);
        // Baseline as of "yesterday" — a real day has since elapsed.
        w.rollover_date = today - ChronoDuration::days(1);
        w.is_focused
            .store(false, std::sync::atomic::Ordering::Relaxed);
        w.maybe_auto_roll();
        assert_eq!(w.anchor, today, "past view should snap home to today");
        assert_eq!(w.rollover_date, today, "baseline resyncs with the anchor");
    }

    /// A focused widget with recent activity must not roll mid-use — the
    /// pending delta is held until the user goes idle.
    #[test]
    fn focused_active_widget_defers_rollover() {
        let mut w = build_widget(CalendarConfig::default());
        let start = Local::now().date_naive() - ChronoDuration::days(1);
        w.anchor = start;
        w.rollover_date = start;
        w.is_focused
            .store(true, std::sync::atomic::Ordering::Relaxed);
        w.last_activity = Instant::now();
        w.maybe_auto_roll();
        assert_eq!(w.anchor, start, "active focused widget must not roll");
        assert_eq!(w.rollover_date, start, "deferral keeps the pending delta");
    }

    /// A user reposition re-bases the rollover baseline so a later
    /// auto-roll advances from where the user landed, not a stale date —
    /// no surprise extra-day drift after pressing a navigation key.
    #[test]
    fn user_navigation_rebases_the_rollover_baseline() {
        let mut w = build_widget(CalendarConfig::default());
        w.rollover_date = Local::now().date_naive() - ChronoDuration::days(3);
        // 'l' / Right advances the anchor by one day in Day view.
        w.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(
            w.rollover_date,
            Local::now().date_naive(),
            "navigating re-bases the baseline to today"
        );
    }
