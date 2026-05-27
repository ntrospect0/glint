// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! ASCII layout-preview sidebar. Drawn alongside the Assign + per-widget
//! pages so the user can see which cell of the dashboard is currently
//! being configured.
//!
//! Each preset declares its grid dimensions and per-cell `(col, row,
//! col_span, row_span)` placement; this module turns that into a set of
//! nested ratatui `Block`s painted into the supplied area. The active
//! cell gets a yellow bold border and inverse-video label; the rest are
//! dim with their assigned widget name (or `(empty)`).

use ratatui::{
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::wizard::{
    pages::layout::{Preset, PRESETS},
    state::{CellAssignment, LayoutChoice},
    style,
};

/// Render the preview into `area`. `active_cell` is the cell index the
/// user is currently configuring (highlight target). `None` ⇒ no cell is
/// active (just show the grid + assignments).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    layout: &LayoutChoice,
    assignments: &[CellAssignment],
    active_cell: Option<usize>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Layout preview ")
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 6 || inner.height < 4 {
        // Too small to draw meaningful cells.
        return;
    }

    let preset = match layout {
        LayoutChoice::Preset { name } => PRESETS.iter().find(|p| p.id == name.as_str()),
        LayoutChoice::KeepExisting => None,
    };
    let Some(preset) = preset else {
        // Keep-existing path: we don't know the cell shape, so just show
        // a textual list. Useful enough for re-runs.
        render_textual_fallback(frame, inner, assignments, active_cell);
        return;
    };

    render_grid(frame, inner, preset, assignments, active_cell);
}

fn render_grid(
    frame: &mut Frame,
    area: Rect,
    preset: &Preset,
    assignments: &[CellAssignment],
    active_cell: Option<usize>,
) {
    // Allow one row at the top for the "configuring cell N" caption.
    let caption_height = 2;
    if area.height < caption_height + 2 {
        return;
    }
    let caption_rect = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: caption_height,
    };
    render_caption(frame, caption_rect, assignments, active_cell);

    let grid_rect = Rect {
        x: area.x,
        y: area.y + caption_height,
        width: area.width,
        height: area.height - caption_height,
    };

    let cell_w = (grid_rect.width / preset.grid_cols as u16).max(3);
    let cell_h = (grid_rect.height / preset.grid_rows as u16).max(3);

    for (i, (col, row, col_span, row_span)) in preset.grid_def.iter().enumerate() {
        let cell_rect = Rect {
            x: grid_rect.x + (*col as u16) * cell_w,
            y: grid_rect.y + (*row as u16) * cell_h,
            width: (*col_span as u16) * cell_w,
            height: (*row_span as u16) * cell_h,
        };
        if cell_rect.width < 2 || cell_rect.height < 2 {
            continue;
        }
        let active = active_cell == Some(i);
        let widget_label = assignments
            .get(i)
            .map(|a| {
                if a.kind.is_empty() {
                    "(empty)".to_string()
                } else {
                    a.widget_id()
                }
            })
            .unwrap_or_else(|| "(empty)".to_string());

        let border_style = if active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let title = if active {
            format!(" ▶ {} ", i + 1)
        } else {
            format!(" {} ", i + 1)
        };
        let title_style = if active {
            style::option_selected()
        } else {
            style::option_idle()
        };
        let cell_block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(title, title_style));
        let cell_inner = cell_block.inner(cell_rect);
        frame.render_widget(cell_block, cell_rect);

        let label_style = if active {
            style::value_focused()
        } else {
            style::value_idle()
        };
        let para = Paragraph::new(widget_label)
            .style(label_style)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        frame.render_widget(para, cell_inner);
    }
}

fn render_caption(
    frame: &mut Frame,
    area: Rect,
    assignments: &[CellAssignment],
    active_cell: Option<usize>,
) {
    let caption = match active_cell {
        Some(idx) => {
            let widget = assignments
                .get(idx)
                .map(|a| {
                    if a.kind.is_empty() {
                        "(empty)".to_string()
                    } else {
                        a.widget_id()
                    }
                })
                .unwrap_or_else(|| "(empty)".to_string());
            Line::from(vec![
                Span::styled("Configuring ", style::blurb()),
                Span::styled(
                    format!("cell {}", idx + 1),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" — {widget}"), style::value_idle()),
            ])
        }
        None => Line::from(Span::styled(
            "Current dashboard layout",
            style::blurb(),
        )),
    };
    frame.render_widget(Paragraph::new(caption), area);
}

fn render_textual_fallback(
    frame: &mut Frame,
    area: Rect,
    assignments: &[CellAssignment],
    active_cell: Option<usize>,
) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Keep-existing layout",
        style::section_header(),
    )));
    lines.push(Line::from(Span::styled(
        "  (cell geometry comes from config.toml)",
        style::blurb(),
    )));
    lines.push(Line::from(""));
    for (i, cell) in assignments.iter().enumerate() {
        let active = active_cell == Some(i);
        let label = if cell.kind.is_empty() {
            "(empty)".to_string()
        } else {
            cell.widget_id()
        };
        let marker = if active { "▶" } else { " " };
        let row_style = if active {
            style::option_selected()
        } else {
            style::option_idle()
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {marker} {}. ", i + 1), row_style),
            Span::styled(label, row_style),
        ]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}
