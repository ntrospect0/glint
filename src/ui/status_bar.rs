// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use chrono::Local;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::theme::Theme;

/// Bottom-of-screen status bar:
/// `glint vX.Y.Z │ [Profile: <name> │] HH:MM:SS │ Focus: <id> │ Scheme: <name> │ Tab: switch · ? help · q quit`
///
/// The `Profile:` segment appears only for a non-default profile, so the
/// default dashboard is visually unchanged.
pub fn render(
    frame: &mut Frame,
    area: Rect,
    focused_widget: Option<&str>,
    scheme_name: &str,
    theme: &Theme,
) {
    let now = Local::now();
    let clock = now.format("%H:%M:%S").to_string();
    let version = env!("CARGO_PKG_VERSION");
    let focus = focused_widget.unwrap_or("—");

    let dim = Style::default().add_modifier(Modifier::DIM);
    let sep = Span::styled("│", dim);

    let mut spans: Vec<Span> = vec![Span::styled(format!(" glint v{version} "), dim)];

    // Active-profile indicator — surfaced right after the version so the
    // context is the first thing read. Hidden for the default profile.
    let profile = crate::config::active_profile();
    if profile != crate::config::DEFAULT_PROFILE {
        spans.push(sep.clone());
        spans.push(Span::styled(" Profile: ", dim));
        spans.push(Span::styled(
            profile.to_string(),
            theme.text_selected.add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" ", dim));
    }

    spans.extend([
        sep.clone(),
        Span::styled(format!(" {clock} "), dim),
        sep.clone(),
        Span::styled(" Focus: ", dim),
        Span::styled(focus.to_string(), theme.text_focused),
        Span::styled(" ", dim),
        sep.clone(),
        Span::styled(" Scheme: ", dim),
        Span::styled(scheme_name.to_string(), theme.text_selected),
        Span::styled(" ", dim),
        sep,
        Span::styled(" Tab: switch · ? help · q quit ", dim),
    ]);

    let line = Line::from(spans).alignment(Alignment::Left);
    frame.render_widget(Paragraph::new(line), area);
}
