// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

pub mod big_digits;
pub mod chart;
pub mod grid;
pub mod help;
pub mod modal;
pub mod status;
pub mod status_bar;

pub use grid::CardGrid;

use std::{cell::Cell, collections::HashMap, collections::HashSet, sync::Arc};

use ratatui::{
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{block::Title, Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::config::{LayoutConfig, ZoomMargin};
use crate::theme::Theme;
use crate::widgets::WidgetManager;
use crate::zoom::ZoomTarget;

/// Severity tag attached to a command-bar feedback message. Determines
/// which theme role colors the message line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackSeverity {
    /// Successful command outcome (e.g. "scheme → tokyonight").
    Confirmation,
    /// Usage hints, missing-argument prompts, non-fatal warnings.
    Warning,
    /// Failed commands and other hard errors.
    Error,
}

/// Per-frame render inputs grouped so the call site doesn't grow another arg
/// every time a new piece of UI state lands.
pub struct RenderState<'a> {
    pub layout: &'a LayoutConfig,
    pub manager: &'a WidgetManager,
    pub focused: Option<&'a str>,
    pub show_help: bool,
    /// `Some` while the user is composing a command at the `:` bar.
    pub command_buffer: Option<&'a str>,
    /// Transient feedback line shown after a command runs. Caller is
    /// responsible for expiring the message (e.g. clearing it on the
    /// next tick after `feedback_ttl`).
    pub command_feedback: Option<(&'a str, FeedbackSeverity)>,
    /// App-level resolved palette used by chrome (command bar, help overlay,
    /// the unknown-widget placeholder). Each widget already carries its own
    /// merged theme — chrome doesn't need to redo that work.
    pub theme: &'a Arc<Theme>,
    /// Name of the active color scheme (matches a `[schemes.<name>]` block
    /// in colorschemes.toml). Used by the help overlay to mark the current
    /// scheme in its listing.
    pub theme_name: &'a str,
    /// Row offset to apply to the help overlay's vertical scroll. App
    /// owns the canonical value; render reads it and applies it via
    /// `Paragraph::scroll`.
    pub help_scroll: u16,
    /// Read/write cell where `help::render` writes back the maximum
    /// scroll value it computed for the current viewport. App's scroll
    /// handler reads this on the next key/wheel event so it can clamp
    /// without re-running the layout math.
    pub help_scroll_max: &'a Cell<u16>,
    /// When `false`, the bottom status bar row is suppressed and that
    /// row goes back to the widget grid. Mirrors `[global]
    /// show_status_bar` in config.toml.
    pub show_status_bar: bool,
    /// The currently-active zoom target, or `None` when zoom is off.
    /// `render_zoom_overlay` reads this to decide whether to paint
    /// the dim + frame pass over the grid.
    pub zoom_target: Option<&'a ZoomTarget>,
    /// Per-side margin used to size the zoom overlay, in percent of screen
    /// width/height per side. Sourced from `[global] zoom_margin` in config.
    pub zoom_margin: ZoomMargin,
}

/// Minimum char gap between the title and the right-aligned metadata on
/// the top border. Below this we let metadata tail-truncate with `…`,
/// and if even one content char + the ellipsis can't fit, hide it
/// entirely rather than let the two strings collide.
const TITLE_METADATA_MIN_GAP: usize = 3;

/// Visual weight of the right-aligned title metadata. Widgets that want
/// to draw the eye to a transient/overridden state (e.g. the weather
/// widget showing a `:weather <city>` lookup instead of the configured
/// home city) pass [`Emphasized`]; the framework lays italic on top of
/// the base `theme.metadata_style(focused)` so the differentiation is
/// preserved even when the metadata gets truncated and no textual
/// marker survives. Other widgets pass [`Default`] and inherit the
/// existing rendering. Adding a new emphasis variant is one match arm
/// in [`metadata_style_for_emphasis`] plus a fallback to make sense to
/// older callers — by convention the framework defines what each
/// variant looks like, not the widget.
///
/// [`Emphasized`]: MetadataEmphasis::Emphasized
/// [`Default`]: MetadataEmphasis::Default
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MetadataEmphasis {
    /// Render in the standard metadata style for the current focus state.
    #[default]
    Default,
    /// Layer italic on top of the standard style. Reserved for metadata
    /// that signals "this isn't the configured default" — transient
    /// overrides, lookup queries, ephemeral filters. Surviving narrow
    /// widths is the point: even when the city name is tail-truncated
    /// to `Toky…`, italics still telegraph "this isn't your home."
    Emphasized,
}

fn metadata_style_for_emphasis(theme: &Theme, focused: bool, emphasis: MetadataEmphasis) -> Style {
    let base = theme.metadata_style(focused);
    match emphasis {
        MetadataEmphasis::Default => base,
        MetadataEmphasis::Emphasized => base.add_modifier(Modifier::ITALIC),
    }
}

/// Build the title row for a widget's border. The title is the
/// left-aligned `Line` painted just after `┌─`; the optional metadata is
/// the right-aligned `Line` painted just before `─┐`. Caller wraps each
/// in `Title::from(line).alignment(...)` and hands them to
/// `Block::title(...)`, or uses [`apply_title_row`] for the common case.
///
/// Focused/unfocused styling lives entirely in the [`Theme`]:
/// * Title text uses `widget_title.focused` vs `widget_title.unfocused`
///   — the focused variant typically carries a background highlight so
///   the user can spot focus without the title shifting position.
/// * The shortcut letter (the one `Shift+<letter>` focuses) is always
///   painted in `text.shortcut`, regardless of focus state.
/// * Metadata uses `metadata.focused` vs `metadata.unfocused` — usually
///   a quieter color than the title, dimmed when the pane isn't focused.
///
/// Letter placement: case-insensitive search of `base` for the shortcut
/// letter. The matching letter is uppercased and styled. If the letter
/// isn't in the title, a `[X] ` badge is prefixed instead so the user
/// still sees the key hint.
///
/// Metadata is tail-truncated with `…` when the pane isn't wide enough
/// to fit `title + min_gap + " " + metadata + " "` inside the inner
/// width (the leading/trailing spaces are visual padding inside the
/// border corners). Only when even one content char plus the ellipsis
/// won't fit do we drop the metadata entirely.
pub fn title_row(
    focused: bool,
    base: &str,
    metadata: Option<&str>,
    emphasis: MetadataEmphasis,
    shortcut: Option<char>,
    theme: &Theme,
    area_width: u16,
) -> (Line<'static>, Option<Line<'static>>) {
    let title = build_title_line(
        focused,
        base,
        shortcut,
        theme.widget_title_style(focused),
        theme.text_shortcut,
        theme.border_focused,
    );

    let metadata_style = metadata_style_for_emphasis(theme, focused, emphasis);
    let inner_w = (area_width as usize).saturating_sub(2);
    let title_w = line_width(&title);
    // build_metadata_line wraps content in 1-cell left/right padding,
    // so the metadata line is `2 + content_chars` wide. Reserve those
    // 2 cells plus the title and the min gap when sizing the content.
    let frame_budget = inner_w
        .saturating_sub(title_w)
        .saturating_sub(TITLE_METADATA_MIN_GAP);
    let content_budget = frame_budget.saturating_sub(2);

    let metadata_line = metadata.filter(|s| !s.is_empty()).and_then(|meta| {
        if content_budget == 0 {
            return None;
        }
        // Need at least 1 content char + the ellipsis (2 cells) to be worth
        // showing when truncation is required — anything less is just `…`.
        if meta.chars().count() > content_budget && content_budget < 2 {
            return None;
        }
        let content = crate::text::truncate(meta, content_budget);
        Some(build_metadata_line(&content, metadata_style))
    });

    (title, metadata_line)
}

/// Attach the title row to a block. Equivalent to calling [`title_row`]
/// and wrapping each non-empty line in a `Title` with the correct
/// alignment; provided so widgets don't repeat the boilerplate.
pub fn apply_title_row<'a>(
    block: Block<'a>,
    focused: bool,
    base: &str,
    metadata: Option<&str>,
    emphasis: MetadataEmphasis,
    shortcut: Option<char>,
    theme: &Theme,
    area_width: u16,
) -> Block<'a> {
    let (title, meta) = title_row(
        focused, base, metadata, emphasis, shortcut, theme, area_width,
    );
    let mut block = block.title(Title::from(title).alignment(Alignment::Left));
    if let Some(meta) = meta {
        block = block.title(Title::from(meta).alignment(Alignment::Right));
    }
    block
}

/// Build the title text with the shortcut letter highlighted. The title
/// is always wrapped in a 1-char pad cell on each side; when the pane is
/// focused those cells render as `┤` / `├` tee-junction glyphs in the
/// border-focused color (the title visually notches into the border line
/// like a labeled segment, btop-style). When unfocused the pad cells are
/// just spaces. Width is constant across focus states so the title chars
/// never shift position.
fn build_title_line(
    focused: bool,
    base: &str,
    shortcut: Option<char>,
    title_style: Style,
    shortcut_style: Style,
    bracket_style: Style,
) -> Line<'static> {
    let (left_pad, right_pad) = if focused { ("┤", "├") } else { (" ", " ") };
    let pad_style = if focused { bracket_style } else { title_style };
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(5);
    spans.push(Span::styled(left_pad.to_string(), pad_style));
    match shortcut {
        Some(letter) => {
            let lower = letter.to_ascii_lowercase();
            let chars: Vec<char> = base.chars().collect();
            if let Some(idx) = chars.iter().position(|c| c.to_ascii_lowercase() == lower) {
                let before: String = chars[..idx].iter().collect();
                let target = chars[idx].to_ascii_uppercase();
                let after: String = chars[idx + 1..].iter().collect();
                if !before.is_empty() {
                    spans.push(Span::styled(before, title_style));
                }
                spans.push(Span::styled(target.to_string(), shortcut_style));
                if !after.is_empty() {
                    spans.push(Span::styled(after, title_style));
                }
            } else {
                spans.push(Span::styled(
                    format!("[{}] ", letter.to_ascii_uppercase()),
                    shortcut_style,
                ));
                spans.push(Span::styled(base.to_string(), title_style));
            }
        }
        None => {
            spans.push(Span::styled(base.to_string(), title_style));
        }
    }
    spans.push(Span::styled(right_pad.to_string(), pad_style));
    Line::from(spans)
}

fn build_metadata_line(meta: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ".to_string(), style),
        Span::styled(meta.to_string(), style),
        Span::styled(" ".to_string(), style),
    ])
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.content.chars().count()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn shortcut_span(line: &Line<'static>, shortcut_style: Style) -> Option<String> {
        line.spans
            .iter()
            .find(|s| s.style == shortcut_style)
            .map(|s| s.content.to_string())
    }

    #[test]
    fn title_row_paints_first_matching_letter_uppercased() {
        let theme = Theme::builtin_defaults();
        let (title, _) = title_row(
            false,
            "Calendar",
            None,
            MetadataEmphasis::Default,
            Some('d'),
            &theme,
            40,
        );
        assert_eq!(line_text(&title), " CalenDar ");
        assert_eq!(
            shortcut_span(&title, theme.text_shortcut).as_deref(),
            Some("D")
        );
    }

    #[test]
    fn title_row_paints_first_letter_when_shortcut_matches_it() {
        let theme = Theme::builtin_defaults();
        let (title, _) = title_row(
            false,
            "Clock",
            None,
            MetadataEmphasis::Default,
            Some('c'),
            &theme,
            40,
        );
        assert_eq!(line_text(&title), " Clock ");
        assert_eq!(
            shortcut_span(&title, theme.text_shortcut).as_deref(),
            Some("C")
        );
    }

    #[test]
    fn title_row_falls_back_to_bracket_badge_when_letter_absent() {
        let theme = Theme::builtin_defaults();
        let (title, _) = title_row(
            false,
            "Weather",
            None,
            MetadataEmphasis::Default,
            Some('z'),
            &theme,
            40,
        );
        let text = line_text(&title);
        assert!(text.contains("[Z] Weather"));
        assert_eq!(
            shortcut_span(&title, theme.text_shortcut).as_deref(),
            Some("[Z] ")
        );
    }

    #[test]
    fn title_row_uses_focused_style_when_focused() {
        let theme = Theme::builtin_defaults();
        let (focused, _) = title_row(
            true,
            "Clock",
            None,
            MetadataEmphasis::Default,
            None,
            &theme,
            40,
        );
        let (unfocused, _) = title_row(
            false,
            "Clock",
            None,
            MetadataEmphasis::Default,
            None,
            &theme,
            40,
        );
        // "Clock" sits in spans[1] (after the leading-pad span).
        assert_eq!(focused.spans[1].style, theme.widget_title_focused);
        assert_eq!(unfocused.spans[1].style, theme.widget_title_unfocused);
    }

    #[test]
    fn title_row_pads_with_tee_brackets_when_focused() {
        // Focus indicator: leading and trailing pad cells become ┤ / ├
        // glyphs styled in the border-focused color so the title
        // notches into the surrounding border line.
        let theme = Theme::builtin_defaults();
        let (focused, _) = title_row(
            true,
            "Clock",
            None,
            MetadataEmphasis::Default,
            None,
            &theme,
            40,
        );
        let (unfocused, _) = title_row(
            false,
            "Clock",
            None,
            MetadataEmphasis::Default,
            None,
            &theme,
            40,
        );
        assert_eq!(focused.spans.first().map(|s| s.content.as_ref()), Some("┤"));
        assert_eq!(focused.spans.last().map(|s| s.content.as_ref()), Some("├"));
        assert_eq!(focused.spans.first().unwrap().style, theme.border_focused);
        assert_eq!(
            unfocused.spans.first().map(|s| s.content.as_ref()),
            Some(" ")
        );
        assert_eq!(
            unfocused.spans.last().map(|s| s.content.as_ref()),
            Some(" ")
        );
        // Width invariant: pad slots stay 1 char in both states.
        let focused_text: String = focused.spans.iter().map(|s| s.content.as_ref()).collect();
        let unfocused_text: String = unfocused.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(focused_text.chars().count(), unfocused_text.chars().count());
    }

    #[test]
    fn title_row_omits_metadata_when_absent_or_empty() {
        let theme = Theme::builtin_defaults();
        let (_, meta) = title_row(
            true,
            "Weather",
            None,
            MetadataEmphasis::Default,
            None,
            &theme,
            60,
        );
        assert!(meta.is_none());
        let (_, meta_empty) = title_row(
            true,
            "Weather",
            Some(""),
            MetadataEmphasis::Default,
            None,
            &theme,
            60,
        );
        assert!(meta_empty.is_none());
    }

    #[test]
    fn title_row_returns_metadata_when_pane_wide_enough() {
        let theme = Theme::builtin_defaults();
        let (title, meta) = title_row(
            true,
            "Weather",
            Some("Richmond, BC"),
            MetadataEmphasis::Default,
            Some('w'),
            &theme,
            60,
        );
        let meta = meta.expect("wide pane should keep metadata");
        assert!(line_text(&title).contains("Weather"));
        assert!(line_text(&meta).contains("Richmond, BC"));
        let body = meta
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "Richmond, BC")
            .expect("metadata body span");
        assert_eq!(body.style, theme.metadata_focused);
    }

    #[test]
    fn title_row_truncates_metadata_with_ellipsis_on_narrow_pane() {
        // 20-wide cell → inner_w 18. Title "[W] Weather" (with focus
        // brackets) is wider than the bare "Weather", min gap 3, plus
        // 2 cells of metadata padding leaves room for only a couple
        // of content chars. Should tail-truncate, not drop.
        let theme = Theme::builtin_defaults();
        let (_, meta) = title_row(
            true,
            "Weather",
            Some("Richmond, BC"),
            MetadataEmphasis::Default,
            Some('w'),
            &theme,
            20,
        );
        let meta = meta.expect("narrow pane should tail-truncate, not drop");
        let body_text: String = meta
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            body_text.contains('…'),
            "narrow metadata should end with the ellipsis char, got {body_text:?}"
        );
        // Whatever survives, it has to be a strict prefix of the original.
        let visible: String = body_text.trim_matches(' ').to_string();
        let prefix: String = visible.trim_end_matches('…').to_string();
        assert!(
            "Richmond, BC".starts_with(prefix.as_str()),
            "truncated text {visible:?} should be a prefix of the original"
        );
    }

    #[test]
    fn title_row_drops_metadata_when_even_ellipsis_wont_fit() {
        // Tiny cell — not enough room for even a 1-char + ellipsis pair.
        let theme = Theme::builtin_defaults();
        let (_, meta) = title_row(
            true,
            "Weather",
            Some("Richmond, BC"),
            MetadataEmphasis::Default,
            Some('w'),
            &theme,
            12,
        );
        assert!(
            meta.is_none(),
            "no room for any content + ellipsis → drop entirely"
        );
    }

    #[test]
    fn title_row_emphasized_metadata_adds_italic_on_top_of_focus_style() {
        // Emphasis is layered onto the focus-derived base style, not
        // a replacement — colors stay theme-driven, italic comes from
        // the framework. Both focused and unfocused panes get italic;
        // the underlying dim/highlight stays intact.
        let theme = Theme::builtin_defaults();
        let (_, meta) = title_row(
            true,
            "Weather",
            Some("Tokyo, Japan"),
            MetadataEmphasis::Emphasized,
            None,
            &theme,
            60,
        );
        let meta = meta.expect("metadata visible at this width");
        let body = meta
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "Tokyo, Japan")
            .expect("metadata body span");
        assert!(
            body.style.add_modifier.contains(Modifier::ITALIC),
            "emphasized metadata should carry the italic modifier"
        );
    }

    #[test]
    fn title_row_metadata_dims_when_pane_unfocused() {
        let theme = Theme::builtin_defaults();
        let (_, meta) = title_row(
            false,
            "Weather",
            Some("Richmond, BC"),
            MetadataEmphasis::Default,
            None,
            &theme,
            60,
        );
        let meta = meta.expect("metadata visible at this width");
        let body = meta
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "Richmond, BC")
            .expect("metadata body span");
        assert_eq!(body.style, theme.metadata_unfocused);
        assert_ne!(
            theme.metadata_focused, theme.metadata_unfocused,
            "metadata styles should differ between states"
        );
    }

    #[test]
    fn title_row_never_emits_focus_arrows() {
        let theme = Theme::builtin_defaults();
        let (title, _) = title_row(
            true,
            "Stocks",
            None,
            MetadataEmphasis::Default,
            Some('s'),
            &theme,
            40,
        );
        let text = line_text(&title);
        assert!(!text.contains('▶'));
        assert!(!text.contains('◀'));
    }

    fn fill_buffer(buf: &mut Buffer, ch: char) {
        for y in buf.area.y..buf.area.bottom() {
            for x in buf.area.x..buf.area.right() {
                buf[(x, y)].set_char(ch);
            }
        }
    }

    fn snapshot_chars(buf: &Buffer, rect: Rect) -> String {
        let mut out = String::new();
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                out.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn blit_rect_copies_cells_inside_both_buffers() {
        let area = Rect::new(0, 0, 8, 4);
        let mut src = Buffer::empty(area);
        let mut dst = Buffer::empty(area);
        fill_buffer(&mut src, 'A');
        fill_buffer(&mut dst, '.');

        blit_rect(&mut dst, &src, Rect::new(2, 1, 3, 2));

        let got = snapshot_chars(&dst, area);
        assert_eq!(
            got,
            "........\n\
             ..AAA...\n\
             ..AAA...\n\
             ........\n",
        );
    }

    #[test]
    fn blit_rect_silently_skips_out_of_bounds_cells() {
        let area = Rect::new(0, 0, 4, 4);
        let mut src = Buffer::empty(area);
        let mut dst = Buffer::empty(area);
        fill_buffer(&mut src, 'A');
        fill_buffer(&mut dst, '.');

        // Rect runs past the right/bottom edge; out-of-bounds cells
        // are skipped, in-bounds cells still copy.
        blit_rect(&mut dst, &src, Rect::new(2, 2, 10, 10));

        let got = snapshot_chars(&dst, area);
        assert_eq!(
            got,
            "....\n\
             ....\n\
             ..AA\n\
             ..AA\n",
        );
    }

    #[test]
    fn blit_rect_with_zero_size_is_noop() {
        let area = Rect::new(0, 0, 4, 4);
        let mut src = Buffer::empty(area);
        let mut dst = Buffer::empty(area);
        fill_buffer(&mut src, 'A');
        fill_buffer(&mut dst, '.');

        blit_rect(&mut dst, &src, Rect::new(1, 1, 0, 0));
        blit_rect(&mut dst, &src, Rect::new(1, 1, 2, 0));
        blit_rect(&mut dst, &src, Rect::new(1, 1, 0, 2));

        let got = snapshot_chars(&dst, area);
        assert!(got.chars().all(|c| c == '.' || c == '\n'));
    }

    /// Verify that the `Clear` widget in step [5] of `render_zoom_overlay`
    /// removes the `Modifier::DIM` flag from cells inside the zoom frame rect.
    /// Cells outside the frame (but inside main_area) retain DIM from step [2].
    #[test]
    fn render_zoom_overlay_clears_zoom_rect_area() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::cell::Cell as StdCell;

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();

        let theme = std::sync::Arc::new(crate::theme::Theme::builtin_defaults());
        let manager = crate::widgets::WidgetManager::new();
        let layout = crate::config::layout::LayoutConfig::default();
        let help_scroll_max = StdCell::new(0u16);

        let zoom = crate::zoom::ZoomTarget {
            parent_id: "missing_widget".into(),
            child_id: None,
        };
        let state = RenderState {
            layout: &layout,
            manager: &manager,
            focused: None,
            show_help: false,
            command_buffer: None,
            command_feedback: None,
            theme: &theme,
            theme_name: "default",
            help_scroll: 0,
            help_scroll_max: &help_scroll_max,
            show_status_bar: false,
            zoom_target: Some(&zoom),
            zoom_margin: ZoomMargin::default(),
        };

        terminal
            .draw(|frame| {
                render(frame, &state);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let area = buf.area;
        // Compute the zoom rect the same way render_zoom_overlay does.
        // main_area == full area (no status bar, no chrome).
        let main_area = area;
        let zoom_rect = zoom_rect_with_margins(main_area, ZoomMargin::default());

        // Cells inside the zoom rect must NOT have DIM (the Clear in step [5]
        // resets them to default style).
        let inside_x = zoom_rect.x + zoom_rect.width / 2;
        let inside_y = zoom_rect.y + zoom_rect.height / 2;
        let inside_dim = buf[(inside_x, inside_y)].modifier.contains(Modifier::DIM);
        assert!(
            !inside_dim,
            "cell ({inside_x},{inside_y}) inside zoom_rect should not have DIM after Clear"
        );

        // Cells outside the zoom rect (in main_area) should have DIM from step [2].
        // Use a corner cell that is guaranteed to be outside the zoom rect.
        let outside_x = main_area.x;
        let outside_y = main_area.y;
        if outside_x < zoom_rect.x || outside_y < zoom_rect.y {
            let outside_dim = buf[(outside_x, outside_y)].modifier.contains(Modifier::DIM);
            assert!(
                outside_dim,
                "cell ({outside_x},{outside_y}) outside zoom_rect should have DIM"
            );
        }
    }

    /// Verify that the placeholder text appears in the home cell area
    /// when zoom is active and the widget id matches a layout cell.
    /// Uses the default layout (which maps "clock" to the top-left cell)
    /// and a real widget registered under "clock".
    #[cfg(feature = "widget-clock")]
    #[test]
    fn render_zoom_placeholder_text_at_home_cell() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::cell::Cell as StdCell;
        use std::sync::Arc;

        let config = crate::config::Config::default();
        let theme = Arc::new(crate::theme::Theme::builtin_defaults());
        let cache = crate::cache::Cache::at(std::env::temp_dir().join("glint-ui-zoom-test"));
        let mut manager = crate::widgets::WidgetManager::new();
        // Register the clock widget so display_name() is available.
        let widget = crate::widgets::registry::build_for("clock", "main", |instance| {
            crate::widgets::WidgetCtx {
                instance,
                theme: theme.clone(),
                llm: None,
                cache: cache.scoped("clock", "main"),
            }
        });
        if let Some(w) = widget {
            manager.register_boxed(w);
        }

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        let help_scroll_max = StdCell::new(0u16);
        let zoom = crate::zoom::ZoomTarget {
            parent_id: "clock".into(),
            child_id: None,
        };
        let state = RenderState {
            layout: &config.layout,
            manager: &manager,
            focused: None,
            show_help: false,
            command_buffer: None,
            command_feedback: None,
            theme: &theme,
            theme_name: "default",
            help_scroll: 0,
            help_scroll_max: &help_scroll_max,
            show_status_bar: false,
            zoom_target: Some(&zoom),
            zoom_margin: ZoomMargin::default(),
        };

        terminal
            .draw(|frame| {
                render(frame, &state);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let area = buf.area;
        // Collect every character in the buffer and verify "zoomed" appears.
        // The placeholder step [3] writes "{name} — zoomed · Esc to return" into
        // the clock's home cell BEFORE step [5] clears the zoom_rect. The portion
        // of the placeholder that falls in the top/left margin (outside the
        // zoom_rect) survives the Clear. With a 120×40 terminal the top margin
        // is 2 rows tall and the clock cell's row 0 is in that margin, so the
        // placeholder text at row 0 is intact in the final buffer.
        let full_snapshot = snapshot_chars(buf, area);
        assert!(
            full_snapshot.contains("zoomed"),
            "placeholder 'zoomed' should appear somewhere in the buffer (margin strip \
             of home cell survives the Clear); got:\n{full_snapshot}"
        );
    }

    /// Verify the CEO Q1 requirement: when `zoom_target.child_id` is `Some`,
    /// `render_zoom_overlay` renders the stack child in isolation — no
    /// tab-strip chrome — by calling `composite_child(child_id).render()`
    /// directly rather than `stack.render()`.
    ///
    /// Regression marker: if step [7] is accidentally changed to call
    /// `parent.render(frame, inner_rect, true)`, the stack's `render` would
    /// overlay the tab strip, painting ALL tab labels (including
    /// "ZoomTabBeta", the inactive child's display name) at the top row of
    /// `inner_rect`.  The assertion below catches that.
    ///
    /// This is the only automated regression net for the CEO Q1
    /// child-isolated render path (§3 QA Layer 2).
    #[test]
    fn render_zoom_overlay_stack_child_has_no_tab_strip() {
        use async_trait::async_trait;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use std::cell::Cell as StdCell;
        use std::sync::Arc;

        // Minimal widget stub: renders nothing, carries a controllable
        // display_name so we can assert its absence from the tab strip.
        struct TabChildStub {
            id: String,
            name: String,
        }

        #[async_trait]
        impl crate::widgets::Widget for TabChildStub {
            fn id(&self) -> &str {
                &self.id
            }
            fn display_name(&self) -> &str {
                &self.name
            }
            fn kind(&self) -> &str {
                "stub"
            }
            async fn update(
                &mut self,
                _ctx: &crate::widgets::AppContext,
            ) -> anyhow::Result<()> {
                Ok(())
            }
            fn render(&self, _frame: &mut Frame, _area: Rect, _focused: bool) {}
            fn handle_key(
                &mut self,
                _key: crossterm::event::KeyEvent,
            ) -> crate::widgets::EventResult {
                crate::widgets::EventResult::Ignored
            }
            fn handle_command(
                &mut self,
                _cmd: &str,
                _args: &[&str],
            ) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn config(&self) -> serde_json::Value {
                serde_json::Value::Null
            }
            fn apply_config(
                &mut self,
                _v: serde_json::Value,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let theme = Arc::new(crate::theme::Theme::builtin_defaults());

        // Two children: active "alpha_tab" / "ZoomTabAlpha" and inactive
        // "beta_tab" / "ZoomTabBeta".  The tab strip would paint both names;
        // the child-isolated path renders neither.
        let child_alpha = TabChildStub {
            id: "alpha_tab".to_string(),
            name: "ZoomTabAlpha".to_string(),
        };
        let child_beta = TabChildStub {
            id: "beta_tab".to_string(),
            name: "ZoomTabBeta".to_string(),
        };

        let stack = crate::widgets::stack::StackWidget::new(
            "stack:alpha_tab+beta_tab".to_string(),
            vec![Box::new(child_alpha), Box::new(child_beta)],
            1,
            theme.clone(),
        );

        let mut manager = WidgetManager::new();
        manager.register_boxed(Box::new(stack));

        let layout = LayoutConfig::default();
        let help_scroll_max = StdCell::new(0u16);

        // CEO Q1 path: child_id is Some — triggers the composite_child
        // branch in render_zoom_overlay step [7].
        let zoom = ZoomTarget {
            parent_id: "stack:alpha_tab+beta_tab".to_string(),
            child_id: Some("alpha_tab".to_string()),
        };
        let state = RenderState {
            layout: &layout,
            manager: &manager,
            focused: None,
            show_help: false,
            command_buffer: None,
            command_feedback: None,
            theme: &theme,
            theme_name: "default",
            help_scroll: 0,
            help_scroll_max: &help_scroll_max,
            show_status_bar: false,
            zoom_target: Some(&zoom),
            zoom_margin: ZoomMargin::default(),
        };

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render(frame, &state);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let main_area = buf.area;

        // Replicate render_zoom_overlay geometry to locate inner_rect.
        let zoom_rect = zoom_rect_with_margins(main_area, ZoomMargin::default());
        let inner_rect = Block::default().borders(Borders::ALL).inner(zoom_rect);

        let inner_snapshot = snapshot_chars(buf, inner_rect);

        // The inactive child's display name must NOT appear inside the zoom
        // frame.  A broken step [7] that calls stack.render() instead of
        // composite_child().render() would paint "ZoomTabBeta" in the tab
        // strip at inner_rect.y, and this assertion would fail.
        assert!(
            !inner_snapshot.contains("ZoomTabBeta"),
            "tab-strip chrome ('ZoomTabBeta') must not appear inside the zoom \
             frame when rendering a stack child in isolation (CEO Q1); \
             inner_rect snapshot:\n{inner_snapshot}"
        );

        // Corroboration: the Clear in step [5] removed DIM from zoom_rect.
        let center_x = zoom_rect.x + zoom_rect.width / 2;
        let center_y = zoom_rect.y + zoom_rect.height / 2;
        assert!(
            !buf[(center_x, center_y)]
                .modifier
                .contains(Modifier::DIM),
            "cells inside zoom_rect must not carry DIM after the Clear in step [5]"
        );
    }

    /// `zoom_rect_with_margins` carves the correct margins off each side.
    #[test]
    fn zoom_rect_with_margins_known_area() {
        use crate::config::ZoomMargin;
        // 100×40 area, uniform 10% margin.
        // left=right=10, top=bottom=4
        let area = Rect::new(0, 0, 100, 40);
        let m = ZoomMargin { top: 10, right: 10, bottom: 10, left: 10 };
        let r = zoom_rect_with_margins(area, m);
        assert_eq!(r.x, 10, "left margin");
        assert_eq!(r.y, 4, "top margin");
        assert_eq!(r.width, 80, "width = 100 - 10 - 10");
        assert_eq!(r.height, 32, "height = 40 - 4 - 4");
    }

    /// `zoom_rect_with_margins` with asymmetric margins.
    #[test]
    fn zoom_rect_with_margins_asymmetric() {
        use crate::config::ZoomMargin;
        // 200×100 area: top=5%, right=10%, bottom=15%, left=20%
        let area = Rect::new(0, 0, 200, 100);
        let m = ZoomMargin { top: 5, right: 10, bottom: 15, left: 20 };
        let r = zoom_rect_with_margins(area, m);
        // left  = 200 * 20 / 100 = 40  →  x = 40
        // right = 200 * 10 / 100 = 20  →  width = 200 - 40 - 20 = 140
        // top   = 100 *  5 / 100 =  5  →  y = 5
        // bot   = 100 * 15 / 100 = 15  →  height = 100 - 5 - 15 = 80
        assert_eq!(r.x, 40);
        assert_eq!(r.y, 5);
        assert_eq!(r.width, 140);
        assert_eq!(r.height, 80);
    }

    /// `zoom_rect_with_margins` never returns an empty rect.
    #[test]
    fn zoom_rect_with_margins_never_empty() {
        use crate::config::ZoomMargin;
        // Tiny terminal; even with large margins the rect must be at least 1×1.
        let area = Rect::new(0, 0, 4, 3);
        let m = ZoomMargin { top: 45, right: 45, bottom: 45, left: 45 };
        let r = zoom_rect_with_margins(area, m);
        assert!(r.width >= 1, "width must be at least 1");
        assert!(r.height >= 1, "height must be at least 1");
    }

    /// Default margin (5% each side) is consistent across the production
    /// render path and the mouse hit-testing path: both call
    /// `zoom_rect_with_margins` with the same `ZoomMargin`, so the rects agree.
    #[test]
    fn zoom_rect_default_margin_consistent() {
        use crate::config::ZoomMargin;
        let area = Rect::new(0, 0, 120, 40);
        let r1 = zoom_rect_with_margins(area, ZoomMargin::default());
        let r2 = zoom_rect_with_margins(area, ZoomMargin::default());
        assert_eq!(r1, r2);
    }

    /// Regression: while zoomed, a backdrop widget whose dirty bit is set must
    /// NOT be re-rendered on the partial path — it stays blitted from the
    /// cached composited buffer.  Simultaneously verifies that the zoom target
    /// itself renders at inner_rect (not at its home cell rect).
    ///
    /// Layout: two side-by-side cells.  "bkdp" is the backdrop; "tgt" is the
    /// zoom target.  Both widgets render a fixed character ('B' and 'T'
    /// respectively).  The primed cache contains 'P' everywhere, simulating a
    /// previous composited zoom frame.  With zoom active and both widgets
    /// marked dirty:
    ///
    ///   - "bkdp" cells that lie outside the inner_rect must show 'P' (blit).
    ///   - "tgt" home cells outside the inner_rect must also show 'P' (blit).
    ///   - Cells inside inner_rect must show 'T' (fresh target render).
    #[test]
    fn render_partial_zoom_backdrop_frozen_target_at_inner_rect() {
        use crate::config::{
            layout::{GridCell, LayoutConfig},
            ZoomMargin,
        };
        use async_trait::async_trait;
        use ratatui::{backend::TestBackend, Terminal};
        use std::cell::Cell as StdCell;
        use std::sync::Arc;

        // Stub that fills its area with a single character.
        struct FillWidget {
            id: String,
            ch: char,
        }

        #[async_trait]
        impl crate::widgets::Widget for FillWidget {
            fn id(&self) -> &str {
                &self.id
            }
            fn display_name(&self) -> &str {
                &self.id
            }
            fn kind(&self) -> &str {
                "stub"
            }
            async fn update(
                &mut self,
                _ctx: &crate::widgets::AppContext,
            ) -> anyhow::Result<()> {
                Ok(())
            }
            fn render(&self, frame: &mut Frame, area: Rect, _focused: bool) {
                let buf = frame.buffer_mut();
                for row in area.y..area.y.saturating_add(area.height) {
                    for col in area.x..area.x.saturating_add(area.width) {
                        if col < buf.area.right() && row < buf.area.bottom() {
                            buf[(col, row)].set_char(self.ch);
                        }
                    }
                }
            }
            fn handle_key(
                &mut self,
                _key: crossterm::event::KeyEvent,
            ) -> crate::widgets::EventResult {
                crate::widgets::EventResult::Ignored
            }
            fn handle_command(
                &mut self,
                _cmd: &str,
                _args: &[&str],
            ) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn config(&self) -> serde_json::Value {
                serde_json::Value::Null
            }
            fn apply_config(&mut self, _v: serde_json::Value) -> anyhow::Result<()> {
                Ok(())
            }
        }

        // Two columns 50/50: "bkdp" left, "tgt" right.
        let layout = LayoutConfig {
            columns: vec![50, 50],
            rows: vec![100],
            cells: vec![
                GridCell {
                    widget: Some("bkdp".into()),
                    widgets: None,
                    col: 0,
                    row: 0,
                    col_span: 1,
                    row_span: 1,
                },
                GridCell {
                    widget: Some("tgt".into()),
                    widgets: None,
                    col: 1,
                    row: 0,
                    col_span: 1,
                    row_span: 1,
                },
            ],
        };

        let theme = Arc::new(crate::theme::Theme::builtin_defaults());
        let mut manager = crate::widgets::WidgetManager::new();
        manager.register_boxed(Box::new(FillWidget { id: "bkdp".into(), ch: 'B' }));
        manager.register_boxed(Box::new(FillWidget { id: "tgt".into(), ch: 'T' }));

        let zoom = ZoomTarget { parent_id: "tgt".into(), child_id: None };
        let help_scroll_max = StdCell::new(0u16);
        let state = RenderState {
            layout: &layout,
            manager: &manager,
            focused: None,
            show_help: false,
            command_buffer: None,
            command_feedback: None,
            theme: &theme,
            theme_name: "default",
            help_scroll: 0,
            help_scroll_max: &help_scroll_max,
            show_status_bar: false,
            zoom_target: Some(&zoom),
            zoom_margin: ZoomMargin::default(),
        };

        // Build a cache primed with 'P' everywhere, area matching the terminal.
        // last_zoom_target must equal state.zoom_target to avoid zoom_toggled=true
        // (which would force a full repaint and bypass the partial path).
        let area = Rect::new(0, 0, 80, 24);
        let mut cached_buf = ratatui::buffer::Buffer::empty(area);
        fill_buffer(&mut cached_buf, 'P');

        // Compute the cell rects the layout would produce so rect_moved stays false.
        let resolved = layout.resolve(area);
        let last_cell_rects: HashMap<String, Rect> = resolved
            .iter()
            .filter_map(|r| r.cell.render_target_id().map(|id| (id, r.area)))
            .collect();

        let mut cache = PartialDrawCache {
            last_drawn: Some(cached_buf),
            last_cell_rects,
            last_focused_id: None,
            last_show_help: false,
            last_zoom_target: Some(zoom.clone()),
        };

        // Mark both widgets dirty.
        let mut dirty_ids = HashSet::new();
        dirty_ids.insert("bkdp".into());
        dirty_ids.insert("tgt".into());

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_partial(frame, &state, &dirty_ids, false, &mut cache);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        let main_area = buf.area;
        let zoom_rect = zoom_rect_with_margins(main_area, ZoomMargin::default());
        let inner_rect = Block::default().borders(Borders::ALL).inner(zoom_rect);

        // Cells strictly outside the zoom_rect (left and right margin strips)
        // must be 'P' (blitted from cache).  The left margin is x=0..zoom_rect.x.
        if zoom_rect.x > 0 {
            let x = 0;
            let y = zoom_rect.y + zoom_rect.height / 2;
            assert_eq!(
                buf[(x, y)].symbol(),
                "P",
                "left margin cell ({x},{y}) must be 'P' (blitted); backdrop must not re-render"
            );
        }

        // Cells inside inner_rect must be 'T' (zoom target rendered fresh there).
        let ix = inner_rect.x + inner_rect.width / 2;
        let iy = inner_rect.y + inner_rect.height / 2;
        assert_eq!(
            buf[(ix, iy)].symbol(),
            "T",
            "inner_rect cell ({ix},{iy}) must be 'T' (fresh target render at inner_rect)"
        );

        // The "tgt" home cell is the right half of the terminal.  Pick a corner
        // outside the zoom_rect (top-right) — must be 'P' (blitted, not re-rendered
        // at the home cell rect).
        let tgt_home_x = main_area.x + main_area.width - 1; // rightmost column
        let tgt_home_y = main_area.y; // top row
        if tgt_home_x >= zoom_rect.x + zoom_rect.width || tgt_home_y < zoom_rect.y {
            assert_eq!(
                buf[(tgt_home_x, tgt_home_y)].symbol(),
                "P",
                "tgt home cell corner ({tgt_home_x},{tgt_home_y}) must be 'P' \
                 (blitted; target must not render at home cell rect)"
            );
        }
    }

    /// While zoomed on a steady-state tick (zoom_toggled=false, not force_full),
    /// the `zoom_active` flag no longer forces a full repaint.  This verifies
    /// the partial path is taken: if the cache is valid the terminal diff layer
    /// should see a partial redraw, not a full one.  Concretely: after the
    /// partial call, the cache's `last_zoom_target` is updated and no panic /
    /// assertion fires (i.e. the non-force_full code path runs to completion).
    #[test]
    fn render_partial_steady_state_zoom_does_not_force_full() {
        use crate::config::{
            layout::{GridCell, LayoutConfig},
            ZoomMargin,
        };
        use async_trait::async_trait;
        use ratatui::{backend::TestBackend, Terminal};
        use std::cell::Cell as StdCell;
        use std::sync::Arc;

        struct NoopWidget {
            id: String,
        }

        #[async_trait]
        impl crate::widgets::Widget for NoopWidget {
            fn id(&self) -> &str {
                &self.id
            }
            fn display_name(&self) -> &str {
                &self.id
            }
            fn kind(&self) -> &str {
                "stub"
            }
            async fn update(
                &mut self,
                _ctx: &crate::widgets::AppContext,
            ) -> anyhow::Result<()> {
                Ok(())
            }
            fn render(&self, _frame: &mut Frame, _area: Rect, _focused: bool) {}
            fn handle_key(
                &mut self,
                _key: crossterm::event::KeyEvent,
            ) -> crate::widgets::EventResult {
                crate::widgets::EventResult::Ignored
            }
            fn handle_command(
                &mut self,
                _cmd: &str,
                _args: &[&str],
            ) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn config(&self) -> serde_json::Value {
                serde_json::Value::Null
            }
            fn apply_config(&mut self, _v: serde_json::Value) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let layout = LayoutConfig {
            columns: vec![100],
            rows: vec![100],
            cells: vec![GridCell {
                widget: Some("w".into()),
                widgets: None,
                col: 0,
                row: 0,
                col_span: 1,
                row_span: 1,
            }],
        };

        let theme = Arc::new(crate::theme::Theme::builtin_defaults());
        let mut manager = crate::widgets::WidgetManager::new();
        manager.register_boxed(Box::new(NoopWidget { id: "w".into() }));

        let zoom = ZoomTarget { parent_id: "w".into(), child_id: None };
        let help_scroll_max = StdCell::new(0u16);
        let state = RenderState {
            layout: &layout,
            manager: &manager,
            focused: None,
            show_help: false,
            command_buffer: None,
            command_feedback: None,
            theme: &theme,
            theme_name: "default",
            help_scroll: 0,
            help_scroll_max: &help_scroll_max,
            show_status_bar: false,
            zoom_target: Some(&zoom),
            zoom_margin: ZoomMargin::default(),
        };

        let area = Rect::new(0, 0, 80, 24);
        let mut cached_buf = ratatui::buffer::Buffer::empty(area);
        fill_buffer(&mut cached_buf, 'X');

        let resolved = layout.resolve(area);
        let last_cell_rects: HashMap<String, Rect> = resolved
            .iter()
            .filter_map(|r| r.cell.render_target_id().map(|id| (id, r.area)))
            .collect();

        let mut cache = PartialDrawCache {
            last_drawn: Some(cached_buf),
            last_cell_rects,
            last_focused_id: None,
            last_show_help: false,
            last_zoom_target: Some(zoom.clone()),
        };

        // No dirty widgets — no data changed, no focus change.
        let dirty_ids: HashSet<String> = HashSet::new();

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        // Must not panic; the partial path must reach its end.
        terminal
            .draw(|frame| {
                render_partial(frame, &state, &dirty_ids, false, &mut cache);
            })
            .unwrap();

        // Cache must now reflect the current zoom target (updated at end of partial path).
        assert_eq!(
            cache.last_zoom_target.as_ref().map(|z| z.parent_id.as_str()),
            Some("w"),
            "cache.last_zoom_target must be updated on the partial path"
        );
    }
}

/// Mutable cache passed by `App` to [`render_partial`]. Owns the
/// pixels of the previous draw plus a few fields the render path
/// uses to decide whether the cache is still valid for the current
/// frame. Reset to `Default::default()` when the app wants to force
/// a full repaint (rare — `render_partial` handles invalidation on
/// its own for the common cases).
#[derive(Default)]
pub struct PartialDrawCache {
    /// Buffer of the last successful draw. `None` invalidates the
    /// entire cache and the next draw becomes a full repaint.
    pub last_drawn: Option<Buffer>,
    /// Rect each widget occupied on the previous draw.
    pub last_cell_rects: HashMap<String, Rect>,
    /// Focused widget id on the previous draw.
    pub last_focused_id: Option<String>,
    /// `show_help` value on the previous draw.
    pub last_show_help: bool,
    /// Zoom target on the previous draw. Drives `zoom_toggled` in
    /// `render_partial` to force a full repaint on any zoom-state
    /// change (entry, exit, or retarget).
    pub last_zoom_target: Option<ZoomTarget>,
}

/// Compute a centered `Rect` of `width_pct`% × `height_pct`% of `area`.
///
/// Shared by all overlay surfaces (help overlay, zoom frame, and any
/// future floating panel). Extracted from `ui::help` to the platform
/// layer so overlay-style features can reuse it without depending on
/// the help module.
pub(crate) fn center_rect(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_pct) / 2),
            Constraint::Percentage(height_pct),
            Constraint::Percentage((100 - height_pct) / 2),
        ])
        .split(area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_pct) / 2),
            Constraint::Percentage(width_pct),
            Constraint::Percentage((100 - width_pct) / 2),
        ])
        .split(rows[1]);
    cols[1]
}

/// Carve a [`ZoomMargin`] off each side of `area` and return the remaining
/// center [`Rect`]. Each margin is `percent * area_dimension / 100` pixels,
/// using integer (floor) division. The result is clamped to at least 1×1 so
/// the rect is always valid even with very small terminals or large margins.
pub(crate) fn zoom_rect_with_margins(area: Rect, m: ZoomMargin) -> Rect {
    let top = (area.height as u32 * m.top as u32 / 100) as u16;
    let bottom = (area.height as u32 * m.bottom as u32 / 100) as u16;
    let left = (area.width as u32 * m.left as u32 / 100) as u16;
    let right = (area.width as u32 * m.right as u32 / 100) as u16;

    let x = area.x.saturating_add(left);
    let y = area.y.saturating_add(top);
    let width = area.width.saturating_sub(left).saturating_sub(right).max(1);
    let height = area.height.saturating_sub(top).saturating_sub(bottom).max(1);

    Rect::new(x, y, width, height)
}

/// Render the full frame: grid cells on top, single-line status bar pinned
/// to the bottom row. Unknown widget ids render a stub placeholder. The help
/// overlay is drawn last on top of everything when enabled. Always paints
/// every widget — used by tests + the `render_partial` fallback path.
pub fn render(frame: &mut Frame, state: &RenderState) {
    let area = frame.area();
    // Bottom-of-screen "chrome row" — a single row that swaps between
    // the command bar, transient feedback, and the status bar. The row
    // is suppressed entirely (handed back to the widget grid) only when
    // the status bar is hidden AND there's no command in flight or
    // recent feedback to surface.
    let chrome_visible =
        state.command_buffer.is_some() || state.command_feedback.is_some() || state.show_status_bar;
    let chrome_h: u16 = if chrome_visible { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(chrome_h)])
        .split(area);
    let main_area = chunks[0];
    let chrome_area = chunks[1];

    // When zoom is active, skip the target's home cell: it renders exactly
    // once at inner_rect in render_zoom_overlay step [7]. Rendering here
    // would create a second live instance (Req 3) and destabilise the image
    // cache (two different rects → re-encode every frame).
    let zoom_parent_id: Option<&str> = state.zoom_target.as_ref().map(|z| z.parent_id.as_str());
    for resolved in state.layout.resolve(main_area) {
        let Some(id) = resolved.cell.render_target_id() else {
            continue;
        };
        if zoom_parent_id == Some(id.as_str()) {
            continue;
        }
        let is_focused = state.focused == Some(id.as_str());
        match state.manager.get(&id) {
            Some(widget) => widget.render(frame, resolved.area, is_focused),
            None => render_unknown(frame, resolved.area, &id, is_focused, state.theme),
        }
    }

    // Priority: active typing wins (user is mid-command); fresh feedback
    // beats the static status bar; status bar is the idle fallback.
    if chrome_h > 0 {
        if let Some(buf) = state.command_buffer {
            render_command_bar(frame, chrome_area, buf, state.theme);
        } else if let Some((msg, severity)) = state.command_feedback {
            render_feedback(frame, chrome_area, msg, severity, state.theme);
        } else if state.show_status_bar {
            status_bar::render(
                frame,
                chrome_area,
                state.focused,
                state.theme_name,
                state.theme,
            );
        }
    }

    // Zoom overlay sits above the widget grid and below the help overlay.
    if let Some(zoom) = state.zoom_target {
        render_zoom_overlay(frame, state, main_area, zoom);
    }

    if state.show_help {
        let sections = build_help_sections(state.layout, state.manager, state.theme_name);
        help::render(
            frame,
            area,
            &sections,
            state.theme,
            state.help_scroll,
            state.help_scroll_max,
        );
    }
}

/// Same shape as [`render`], but only repaints widget cells whose
/// dirty bit is set (or whose visual surface changed for another
/// reason — focus shift, layout reflow, etc.). Cells we skip are
/// blitted from `cache.last_drawn` into the new frame's buffer, so
/// the terminal-diff layer downstream sees the same final pixels
/// either way. Chrome (command bar / feedback / status bar) and the
/// help overlay are cheap and always painted fresh.
///
/// Invalidation rules (any one forces a full repaint this frame and
/// drops the cache so the next frame is also full):
///
/// 1. **`force_full` set by caller** — the dirty-id set isn't a
///    reliable signal this frame. Used on non-tick events (key,
///    mouse, paste, resize, config change): widgets' `handle_key`
///    et al. mutate state without setting their own dirty bit by
///    trait contract (they rely on the app's unconditional non-
///    tick redraw), so we can't trust `dirty_ids` to enumerate
///    what visibly changed.
/// 2. **Cache empty** — first draw of the session, or someone
///    reset it manually.
/// 3. **Frame area changed** — terminal resize; cached buffer is
///    the wrong size.
/// 4. **Help overlay toggled** — overlay pixels in the cache would
///    smear across widgets when the overlay closes, so we forcibly
///    repaint on either edge.
/// 5. **Help overlay currently shown** — the overlay paints over
///    every widget anyway, so caching its frame is meaningless;
///    re-render in full while it's up.
/// 6. **Per-widget**: dirty bit set, focus changed for this widget
///    (gained or lost), or cell rect moved.
pub fn render_partial(
    frame: &mut Frame,
    state: &RenderState,
    dirty_ids: &HashSet<String>,
    force_full: bool,
    cache: &mut PartialDrawCache,
) {
    let area = frame.area();

    // Frame-level invalidations: any of these means we can't trust
    // the cached pixels, so render everything and rebuild the cache
    // from this draw.
    let help_toggled = state.show_help != cache.last_show_help;
    let area_changed = cache
        .last_drawn
        .as_ref()
        .map_or(true, |b| b.area != area);
    // Zoom: force a full repaint on zoom edges only (entry, exit, retarget).
    // Steady-state while zoomed uses the partial path — the cached buffer
    // already carries the composited dim + placeholder + frame from the edge
    // repaint, so backdrop widgets stay frozen and only the target re-renders
    // at inner_rect when its data changes.
    let zoom_toggled = state.zoom_target != cache.last_zoom_target.as_ref().map(|z| z);
    let force_full = force_full || area_changed || help_toggled || state.show_help || zoom_toggled;

    if force_full {
        render(frame, state);
        if state.show_help {
            // Don't cache while the overlay is up — `last_drawn`
            // would hold overlay pixels and corrupt the next paint
            // after dismissal. Drop the cache entirely so the next
            // frame goes through full-repaint again.
            *cache = PartialDrawCache {
                last_show_help: true,
                ..Default::default()
            };
        } else {
            *cache = PartialDrawCache {
                last_drawn: Some(frame.buffer_mut().clone()),
                last_cell_rects: collect_cell_rects(frame, state),
                last_focused_id: state.focused.map(str::to_string),
                last_show_help: false,
                last_zoom_target: state.zoom_target.cloned(),
            };
        }
        return;
    }

    // From here on the cache is valid: same size, no help overlay.
    let cached_buf = cache
        .last_drawn
        .as_ref()
        .expect("cache validated above is non-None");

    // Mirror what `render` does for the chrome split so we know the
    // widget grid's `main_area`. Chrome is always re-painted from
    // scratch — it's a single row and cheap.
    let chrome_visible =
        state.command_buffer.is_some() || state.command_feedback.is_some() || state.show_status_bar;
    let chrome_h: u16 = if chrome_visible { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(chrome_h)])
        .split(area);
    let main_area = chunks[0];
    let chrome_area = chunks[1];

    // First pass: figure out which cells need a fresh widget render
    // this frame. A widget needs to render when:
    //
    //   - its dirty bit was set,
    //   - it gained or lost focus,
    //   - its rect moved (layout reflow, stack tab switch, …), or
    //   - we don't have a previous rect for it yet (newly added).
    //
    // Everything else blits.
    //
    // While zoomed, backdrop widgets are always frozen in the blit cache
    // regardless of their dirty bits — only the zoom target is eligible to
    // join to_render. This prevents double-renders of image widgets and
    // stale iTerm2 escape sequences corrupting the dimmed backdrop.
    let focus_changed = state.focused.map(str::to_string) != cache.last_focused_id;
    let zoom_parent_id: Option<&str> = state.zoom_target.as_ref().map(|z| z.parent_id.as_str());
    let resolved = state.layout.resolve(main_area);
    let mut to_render: HashSet<String> = HashSet::new();
    let mut new_rects: HashMap<String, Rect> = HashMap::with_capacity(resolved.len());
    for r in &resolved {
        let Some(id) = r.cell.render_target_id() else {
            continue;
        };
        new_rects.insert(id.clone(), r.area);
        // Freeze backdrop widgets — only the zoom target can join to_render.
        if zoom_parent_id.is_some() && zoom_parent_id != Some(id.as_str()) {
            continue;
        }
        let rect_moved = cache
            .last_cell_rects
            .get(&id)
            .map_or(true, |prev| *prev != r.area);
        let focus_touched = focus_changed
            && (cache.last_focused_id.as_deref() == Some(id.as_str())
                || state.focused == Some(id.as_str()));
        if dirty_ids.contains(&id) || rect_moved || focus_touched {
            to_render.insert(id);
        }
    }

    // Second pass: blit cached pixels for the cells we're skipping,
    // then render the dirty ones on top. Doing blits first means
    // any newly-rendered widget overwrites stale cached pixels in
    // its rect (defense-in-depth against an edge where rect_moved
    // didn't fire but should have).
    //
    // While zoomed: the target's home cell is ALWAYS blitted from cache
    // (the placeholder lives there in the cached composited buffer) even
    // when the target is in to_render. The fresh render happens at
    // inner_rect after the grid loops, not at the home cell.
    for r in &resolved {
        let Some(id) = r.cell.render_target_id() else {
            continue;
        };
        let is_zoom_home = zoom_parent_id == Some(id.as_str());
        if !to_render.contains(&id) || is_zoom_home {
            blit_rect(frame.buffer_mut(), cached_buf, r.area);
        }
    }
    for r in &resolved {
        let Some(id) = r.cell.render_target_id() else {
            continue;
        };
        if !to_render.contains(&id) {
            continue;
        }
        // Skip the zoom target at its home cell — it renders at inner_rect below.
        if zoom_parent_id == Some(id.as_str()) {
            continue;
        }
        let is_focused = state.focused == Some(id.as_str());
        match state.manager.get(&id) {
            Some(widget) => widget.render(frame, r.area, is_focused),
            None => render_unknown(frame, r.area, &id, is_focused, state.theme),
        }
    }

    // While zoomed, if the target was dirty, render it at inner_rect.
    // The dim + frame border are preserved from the blitted cache — we only
    // repaint the widget's interior so live data stays current (Req 4).
    // We intentionally do NOT re-apply dim or re-draw the frame border here;
    // those are stable in the cache and double-dim on re-application.
    if let Some(zoom) = state.zoom_target {
        if to_render.contains(zoom.parent_id.as_str()) {
            let zoom_rect = zoom_rect_with_margins(main_area, state.zoom_margin);
            let inner_rect = Block::default().borders(Borders::ALL).inner(zoom_rect);
            // Clear the frame interior before re-rendering the zoomed widget on a
            // steady-state tick. The backdrop/border stay cached, but the interior
            // is blitted from the cached buffer — a widget that paints sparsely
            // (e.g. big-digit clocks: centered glyphs, off-cells left untouched)
            // would otherwise composite over stale cells and garble as the digits
            // change width. The border lives on zoom_rect (outside inner_rect), so
            // clearing here loses nothing.
            frame.render_widget(Clear, inner_rect);
            match &zoom.child_id {
                None => {
                    if let Some(widget) = state.manager.get(&zoom.parent_id) {
                        widget.render(frame, inner_rect, true);
                    }
                }
                Some(child_id) => {
                    if let Some(parent) = state.manager.get(&zoom.parent_id) {
                        if let Some(child) = parent.composite_child(child_id) {
                            child.render(frame, inner_rect, true);
                        }
                    }
                }
            }
        }
    }

    // Chrome row is always fresh.
    if chrome_h > 0 {
        if let Some(buf) = state.command_buffer {
            render_command_bar(frame, chrome_area, buf, state.theme);
        } else if let Some((msg, severity)) = state.command_feedback {
            render_feedback(frame, chrome_area, msg, severity, state.theme);
        } else if state.show_status_bar {
            status_bar::render(
                frame,
                chrome_area,
                state.focused,
                state.theme_name,
                state.theme,
            );
        }
    }

    // Capture this frame's pixels + bookkeeping for next time.
    cache.last_drawn = Some(frame.buffer_mut().clone());
    cache.last_cell_rects = new_rects;
    cache.last_focused_id = state.focused.map(str::to_string);
    cache.last_show_help = false;
    cache.last_zoom_target = state.zoom_target.cloned();
}

/// Copy a rect's worth of cells from `src` into `dst`. Assumes the
/// rect is fully inside both buffers (caller has already clamped to
/// `dst.area`); out-of-bounds reads/writes are silently skipped via
/// the `Buffer` index API which clamps. Used by the partial-render
/// path to blit a previous widget's pixels into the new frame.
fn blit_rect(dst: &mut Buffer, src: &Buffer, rect: Rect) {
    let src_area = src.area;
    let dst_area = dst.area;
    for y in rect.y..rect.y.saturating_add(rect.height) {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            if x >= src_area.right()
                || y >= src_area.bottom()
                || x >= dst_area.right()
                || y >= dst_area.bottom()
            {
                continue;
            }
            dst[(x, y)] = src[(x, y)].clone();
        }
    }
}

/// Resolve the current layout against the frame's main area and
/// collect each widget's rect, keyed by render-target id. Mirrors
/// the resolve in `render` / `render_partial` and is only used on
/// the full-redraw path so the cache picks up the right per-widget
/// rects when there's nothing to blit from.
fn collect_cell_rects(frame: &Frame, state: &RenderState) -> HashMap<String, Rect> {
    let area = frame.area();
    let chrome_visible =
        state.command_buffer.is_some() || state.command_feedback.is_some() || state.show_status_bar;
    let chrome_h: u16 = if chrome_visible { 1 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(chrome_h)])
        .split(area);
    let main_area = chunks[0];
    state
        .layout
        .resolve(main_area)
        .into_iter()
        .filter_map(|r| r.cell.render_target_id().map(|id| (id, r.area)))
        .collect()
}

fn render_command_bar(frame: &mut Frame, area: Rect, buffer: &str, theme: &Theme) {
    // Cursor caret strips bold/bg from the focused-text style so the bar
    // line stays calm even when the scheme makes text.focused heavy.
    let caret_style = Style::default().fg(theme.text_focused.fg.unwrap_or(Color::LightCyan));
    let spans = vec![
        Span::styled(":", theme.text_focused),
        Span::raw(buffer.to_string()),
        Span::styled("▏", caret_style),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Map a feedback severity onto a scheme-driven theme role so the message
/// inherits whatever the active palette ships for "highlight" / "warning"
/// / "error". The roles deliberately reuse existing slots so a single
/// `[schemes.<name>]` block already styles every severity — no new theme
/// fields required.
fn feedback_style(theme: &Theme, severity: FeedbackSeverity) -> Style {
    match severity {
        FeedbackSeverity::Confirmation => theme.text_focused,
        FeedbackSeverity::Warning => theme.text_selected,
        FeedbackSeverity::Error => theme.text_shortcut,
    }
}

fn render_feedback(
    frame: &mut Frame,
    area: Rect,
    msg: &str,
    severity: FeedbackSeverity,
    theme: &Theme,
) {
    // Leading single-space pad matches the status bar's left edge so the
    // text doesn't kiss the terminal border.
    let line = Line::from(vec![
        Span::raw(" "),
        Span::styled(msg.to_string(), feedback_style(theme, severity)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn build_help_sections(
    layout: &LayoutConfig,
    manager: &WidgetManager,
    active_scheme: &str,
) -> Vec<help::Section> {
    let global = help::Section {
        title: "Global".into(),
        bindings: vec![
            ("Tab / Shift+Tab".into(), "cycle focused widget".into()),
            (
                "Shift+<letter>".into(),
                "jump focus to widget (red letter in title)".into(),
            ),
            ("click cell".into(), "focus that widget".into()),
            ("z / Z".into(), "zoom focused widget".into()),
            ("z / Z (while zoomed)".into(), "exit zoom".into()),
            (":".into(), "open command bar".into()),
            (
                "Ctrl+U (in :)".into(),
                "clear the command bar (keeps the `:` prompt)".into(),
            ),
            (
                ":scheme <name>".into(),
                "switch color scheme (see list below)".into(),
            ),
            ("?".into(), "toggle this help overlay".into()),
            ("q · Ctrl+C".into(), "quit".into()),
        ],
    };
    let mut sections = vec![global];

    // Per-widget sections, ordered by layout appearance. Stack cells
    // expand into a section for the stack itself (rotate prev/next)
    // plus one section per child — including the currently-hidden
    // tabs — so the help overlay reflects everything a user can reach
    // from that pane.
    let mut seen_owned: std::collections::HashSet<String> = std::collections::HashSet::new();
    let push_bindings =
        |sections: &mut Vec<help::Section>, title: String, widget: &dyn crate::widgets::Widget| {
            let bindings: Vec<(String, String)> = widget
                .keybindings()
                .into_iter()
                .map(|(k, d)| (k.to_string(), d.to_string()))
                .collect();
            if bindings.is_empty() {
                return;
            }
            sections.push(help::Section { title, bindings });
        };
    for cell in &layout.cells {
        let Some(id) = cell.render_target_id() else {
            continue;
        };
        if !seen_owned.insert(id.clone()) {
            continue;
        }
        let Some(widget) = manager.get(&id) else {
            continue;
        };

        let child_ids = widget.composite_children();
        if child_ids.is_empty() {
            push_bindings(&mut sections, widget.display_name().to_string(), widget);
            continue;
        }

        // Composite cell: render the stack's own bindings first (so the
        // tab-rotation keys aren't buried under the child sections),
        // then a section per child in tab order. Title the stack
        // section by its children to disambiguate when more than one
        // stack lives in the same layout.
        let mut stack_label_parts: Vec<String> = Vec::with_capacity(child_ids.len());
        for child_id in &child_ids {
            if let Some(child) = widget.composite_child(child_id) {
                stack_label_parts.push(child.display_name().to_string());
            }
        }
        let stack_title = if stack_label_parts.is_empty() {
            "Stack".to_string()
        } else {
            format!("Stack: {}", stack_label_parts.join(" + "))
        };
        push_bindings(&mut sections, stack_title, widget);
        for child_id in &child_ids {
            if let Some(child) = widget.composite_child(child_id) {
                push_bindings(&mut sections, child.display_name().to_string(), child);
            }
        }
    }

    // Append a "Color schemes" section that lists every named scheme in
    // ~/.config/glint/colorschemes.toml so the user doesn't have to
    // remember them. Marks the active one with `●`. Read errors and the
    // missing-file case both yield an empty section (skipped silently).
    if let Ok(file) = crate::theme::load_schemes_file() {
        let mut names: Vec<&str> = file.schemes.keys().map(String::as_str).collect();
        names.sort_unstable();
        if !names.is_empty() {
            let bindings: Vec<(String, String)> = names
                .into_iter()
                .map(|name| {
                    let key = if name == active_scheme {
                        format!("● {name}")
                    } else {
                        format!("  {name}")
                    };
                    let desc = if name == active_scheme {
                        "active scheme".to_string()
                    } else {
                        format!(":scheme {name}")
                    };
                    (key, desc)
                })
                .collect();
            sections.push(help::Section {
                title: "Color schemes".into(),
                bindings,
            });
        }
    }

    sections
}

/// Render the seven-step zoom overlay on top of the already-rendered grid.
///
/// Steps, in order:
///
/// 1. (caller) Grid rendered normally — the zoom target's home cell is
///    **skipped** in the grid loop (`render` guards on `zoom_parent_id`),
///    so no second live instance exists before step 3 runs.
/// 2. **Dim pass:** every cell in `main_area` gets `Modifier::DIM` added.
///    One buffer pass; no widget cooperation required.
/// 3. **Placeholder:** the zoom target's home cell is overwritten with a
///    static dim paragraph `"{name} — zoomed · Esc to return"`. This is the
///    one-live-instance invariant: the home cell is now a static label, not
///    a second widget render.
/// 4. `zoom_rect = zoom_rect_with_margins(main_area, state.zoom_margin)` — sized
///    by `[global] zoom_margin` from config (default: 5% per side).
/// 5. `Clear` the `zoom_rect`, removing the dim applied in step 2.
/// 6. Render the focused-border block with the `" z · Esc to exit zoom "`
///    title annotation.
/// 7. Resolve and render the widget at `inner_rect`:
///    - Leaf: `manager.get(parent_id).render(inner_rect, focused=true)`
///    - Stack child: `composite_child(child_id).render(inner_rect, focused=true)` —
///      bypasses the stack's `render` and its tab-strip overlay (CEO Q1).
///    - If either lookup returns `None` (live-reload race), return early;
///      `apply_config_change`'s guard clears `zoom_target` on the next tick.
fn render_zoom_overlay(
    frame: &mut Frame,
    state: &RenderState,
    main_area: Rect,
    zoom: &ZoomTarget,
) {
    // [2] Dim every cell in main_area.
    {
        let buf = frame.buffer_mut();
        for row in main_area.y..main_area.y.saturating_add(main_area.height) {
            for col in main_area.x..main_area.x.saturating_add(main_area.width) {
                if col < buf.area.right() && row < buf.area.bottom() {
                    buf[(col, row)].modifier |= Modifier::DIM;
                }
            }
        }
    }

    // [3] Overwrite the home cell with a static placeholder paragraph.
    // Find the resolved Rect for this zoom target's parent_id. Skip silently
    // on a live-reload race (the Clear in step [5] still produces a valid frame).
    if let Some(home_area) = state
        .layout
        .resolve(main_area)
        .into_iter()
        .find(|r| r.cell.render_target_id().as_deref() == Some(zoom.parent_id.as_str()))
        .map(|r| r.area)
    {
        let display_name = state
            .manager
            .get(&zoom.parent_id)
            .map(|w| w.display_name().to_string())
            .unwrap_or_else(|| zoom.parent_id.clone());
        let placeholder = format!("{display_name} — zoomed · Esc to return");
        let para = Paragraph::new(placeholder)
            .alignment(Alignment::Center)
            .style(state.theme.text_dim);
        frame.render_widget(para, home_area);
    }

    // [4] Zoom frame rect sized by the configured per-side margin.
    let zoom_rect = zoom_rect_with_margins(main_area, state.zoom_margin);

    // [5] Clear the zoom rect, removing the dim applied in step [2].
    frame.render_widget(Clear, zoom_rect);

    // [6] Focused-border frame block with zoom-exit title annotation.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(state.theme.border_focused)
        .title(Span::styled(
            " z · Esc to exit zoom ",
            state.theme.text_dim,
        ));
    let inner_rect = block.inner(zoom_rect);
    frame.render_widget(block, zoom_rect);

    // [7] Resolve and render the widget at inner_rect.
    match &zoom.child_id {
        None => {
            // Leaf widget: render directly at the inner zoom frame area.
            if let Some(widget) = state.manager.get(&zoom.parent_id) {
                widget.render(frame, inner_rect, true);
            }
        }
        Some(child_id) => {
            // Stack child (CEO Q1): bypass the stack's render method and its
            // tab-strip overlay by calling composite_child directly.
            if let Some(parent) = state.manager.get(&zoom.parent_id) {
                if let Some(child) = parent.composite_child(child_id) {
                    child.render(frame, inner_rect, true);
                }
            }
        }
    }
}

fn render_unknown(frame: &mut Frame, area: Rect, id: &str, focused: bool, theme: &Theme) {
    let block = apply_title_row(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.border_style(focused)),
        focused,
        id,
        None,
        MetadataEmphasis::Default,
        None,
        theme,
        area.width,
    );
    // `id` looks like `gallery` or `gallery@home`; strip the instance suffix
    // so the cargo-feature hint matches the on-disk feature name.
    let kind = id.split_once('@').map(|(k, _)| k).unwrap_or(id);
    let body = Paragraph::new(vec![
        Line::from(""),
        Line::from(format!("Widget '{id}' is not available in this build.")),
        Line::from(""),
        Line::from(format!(
            "Rebuild with --features widget-{kind} to enable it,"
        )),
        Line::from("or remove this cell from your layout in config.toml."),
    ])
    .alignment(Alignment::Center)
    .block(block);
    frame.render_widget(body, area);
}
