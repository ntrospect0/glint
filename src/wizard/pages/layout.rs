// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Layout page. Two-phase: first asks for the number of panes the user
//! wants, then surfaces the preset layouts that match that count. The
//! preset list is curated rather than fully customisable — power users
//! who want a bespoke grid edit `config.toml` directly (the confirm page
//! points them at the right block).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use super::PageAction;
use crate::wizard::{
    app::{LayoutPhase, WizardApp},
    state::{CellAssignment, LayoutChoice, WizardState},
    style,
};

/// A named layout offered on this page.
///
/// Loaded once from the embedded `layouts.toml` data file and cached
/// in a `OnceLock`. Field types are owned (rather than the previous
/// `&'static str` / `&'static [..]`) so they can carry parsed data;
/// callers that previously held `&'static Preset` still work because
/// the cache itself lives for the program's lifetime.
#[derive(Debug, serde::Deserialize)]
pub struct Preset {
    pub id: String,
    pub label: String,
    pub description: String,
    /// Number of cells the assign page will produce.
    pub cells: usize,
    /// ASCII preview rendered next to the description on the preset
    /// picker. Static art; the dynamic per-cell preview rendered on
    /// the Assign + per-widget pages uses [`grid_def`] instead.
    pub ascii: Vec<String>,
    /// Grid dimensions for the dynamic preview.
    pub grid_cols: usize,
    pub grid_rows: usize,
    /// One `[col, row, col_span, row_span]` per cell, in registration
    /// order. The Assign page assigns widgets to cells using this
    /// same order, so cell 0 in `grid_def` maps to `assignments[0]`.
    /// TOML carries these as four-element arrays; deserialization
    /// converts to the 4-tuple form code consumes.
    #[serde(deserialize_with = "deserialize_grid_def")]
    pub grid_def: Vec<(usize, usize, usize, usize)>,
}

fn deserialize_grid_def<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> Result<Vec<(usize, usize, usize, usize)>, D::Error> {
    use serde::de::Error;
    let raw: Vec<Vec<usize>> = serde::Deserialize::deserialize(de)?;
    raw.into_iter()
        .map(|v| match v.as_slice() {
            [a, b, c, d] => Ok((*a, *b, *c, *d)),
            other => Err(D::Error::custom(format!(
                "grid_def entry must be a 4-element [col, row, col_span, row_span] array; got {} elements",
                other.len()
            ))),
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct RawPresetFile {
    presets: Vec<Preset>,
}

const PRESETS_TOML: &str = include_str!("layouts.toml");

/// Every layout preset, parsed once from `layouts.toml`. Panics on a
/// malformed embedded TOML — that's a programmer error caught by the
/// test below, not user-supplied data.
pub fn all_presets() -> &'static [Preset] {
    static CACHE: std::sync::OnceLock<Vec<Preset>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let parsed: RawPresetFile = toml::from_str(PRESETS_TOML)
            .unwrap_or_else(|err| panic!("layouts.toml: parse failed: {err}"));
        parsed.presets
    })
}

const MIN_PANES: usize = 1;
const MAX_PANES: usize = 8;

pub fn handle_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    match app.layout_phase {
        LayoutPhase::PickCount => handle_count_key(key, app),
        LayoutPhase::PickPreset => handle_preset_key(key, app),
    }
}

fn handle_count_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    let cur = current_count(app).unwrap_or(default_count(app));
    match key.code {
        KeyCode::Left | KeyCode::Char('h') | KeyCode::Up | KeyCode::Char('k') => {
            let next = if cur > MIN_PANES { cur - 1 } else { MAX_PANES };
            app.text_buffer = next.to_string();
            PageAction::Stay
        }
        KeyCode::Right | KeyCode::Char('l') | KeyCode::Down | KeyCode::Char('j') => {
            let next = if cur < MAX_PANES { cur + 1 } else { MIN_PANES };
            app.text_buffer = next.to_string();
            PageAction::Stay
        }
        KeyCode::Char(c) if c.is_ascii_digit() => {
            let d = (c as u8 - b'0') as usize;
            if (MIN_PANES..=MAX_PANES).contains(&d) {
                app.text_buffer = d.to_string();
            }
            PageAction::Stay
        }
        // KeepExisting option — `k` jumps straight to "use existing
        // layout" and skips preset selection.
        KeyCode::Char('e') | KeyCode::Char('E') => {
            commit_keep_existing(app);
            PageAction::Advance
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let count = current_count(app).unwrap_or(default_count(app));
            app.text_buffer = count.to_string();
            app.layout_phase = LayoutPhase::PickPreset;
            app.focus = 0;
            PageAction::Stay
        }
        KeyCode::Esc => PageAction::Back,
        _ => PageAction::Stay,
    }
}

fn handle_preset_key(key: KeyEvent, app: &mut WizardApp) -> PageAction {
    let count = current_count(app).unwrap_or(default_count(app));
    let matching: Vec<&Preset> = presets_for(count).collect();
    if matching.is_empty() {
        // No matching preset — only Back / "Keep existing" should work.
        return match key.code {
            KeyCode::Esc => {
                app.layout_phase = LayoutPhase::PickCount;
                app.focus = 0;
                PageAction::Stay
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                commit_keep_existing(app);
                PageAction::Advance
            }
            _ => PageAction::Stay,
        };
    }
    let n = matching.len();
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.focus = (app.focus + n - 1) % n;
            PageAction::Stay
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.focus = (app.focus + 1) % n;
            PageAction::Stay
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let preset = matching[app.focus.min(n - 1)];
            commit_preset(app, preset);
            PageAction::Advance
        }
        KeyCode::Esc => {
            // Back from preset selection returns to count picker, not the
            // global page — the user usually just wants to bump the count.
            app.layout_phase = LayoutPhase::PickCount;
            app.focus = 0;
            PageAction::Stay
        }
        _ => PageAction::Stay,
    }
}

fn commit_preset(app: &mut WizardApp, preset: &Preset) {
    let choice = LayoutChoice::Preset {
        name: preset.id.clone(),
    };
    seed_assignments(&mut app.state, &choice);
    app.state.layout = choice;
}

fn commit_keep_existing(app: &mut WizardApp) {
    app.state.layout = LayoutChoice::KeepExisting;
}

/// Pre-fill the assignments list with empty slots for the chosen preset so
/// the Assign page can iterate cells without computing them itself.
fn seed_assignments(state: &mut WizardState, choice: &LayoutChoice) {
    let want = match choice {
        LayoutChoice::Preset { name } => all_presets()
            .iter()
            .find(|p| &p.id == name)
            .map(|p| p.cells)
            .unwrap_or(0),
        LayoutChoice::KeepExisting => return,
    };
    state.assignments.truncate(want);
    while state.assignments.len() < want {
        state.assignments.push(CellAssignment {
            cell_index: state.assignments.len(),
            kind: String::new(),
            instance: "main".into(),
            stack_children: Vec::new(),
        });
    }
}

fn current_count(app: &WizardApp) -> Option<usize> {
    app.text_buffer
        .parse::<usize>()
        .ok()
        .filter(|n| (MIN_PANES..=MAX_PANES).contains(n))
}

fn default_count(app: &WizardApp) -> usize {
    // On re-entry, default to whatever count the user previously chose.
    // For KeepExisting, use the assignment count we hydrated from
    // config.toml so the count picker starts on the user's actual layout
    // size rather than an arbitrary fallback.
    match &app.state.layout {
        LayoutChoice::Preset { name } => all_presets()
            .iter()
            .find(|p| &p.id == name)
            .map(|p| p.cells)
            .unwrap_or(4),
        LayoutChoice::KeepExisting => app.state.assignments.len().clamp(MIN_PANES, MAX_PANES),
    }
}

fn presets_for(count: usize) -> impl Iterator<Item = &'static Preset> {
    all_presets().iter().filter(move |p| p.cells == count)
}

pub fn render(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let title = match app.layout_phase {
        LayoutPhase::PickCount => " Layout — step 1 of 2: how many panes? ",
        LayoutPhase::PickPreset => " Layout — step 2 of 2: choose a preset ",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = style::pad_inner(block.inner(area));
    frame.render_widget(block, area);

    match app.layout_phase {
        LayoutPhase::PickCount => render_count_picker(frame, inner, app),
        LayoutPhase::PickPreset => render_preset_picker(frame, inner, app),
    }
}

fn render_count_picker(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let count = current_count(app).unwrap_or(default_count(app));
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "How many widgets do you want on the dashboard?",
        style::section_header(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "  Each pane holds one widget. Pick a count from {} to {}; the next \
             step offers preset grid shapes that fit it.",
            MIN_PANES, MAX_PANES
        ),
        style::blurb(),
    )));
    lines.push(Line::from(""));

    // Big count display.
    lines.push(Line::from(vec![
        Span::raw("      "),
        Span::styled("◀ ", style::key_hint()),
        Span::styled(
            format!("{count} pane{} ", if count == 1 { "" } else { "s" }),
            style::value_focused(),
        ),
        Span::styled("▶", style::key_hint()),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("       Press 1–{MAX_PANES} or use ←/→ ↑/↓ to adjust. Enter to continue."),
        style::help_text(),
    )));
    lines.push(Line::from(""));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Already happy with your existing layout?",
        style::label(),
    )));
    lines.push(Line::from(vec![
        Span::raw("      "),
        Span::styled("[E]", style::key_hint()),
        Span::raw(" "),
        Span::styled(
            "Keep existing — leaves the [layout] block in config.toml untouched.",
            style::value_idle(),
        ),
    ]));

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_preset_picker(frame: &mut Frame, area: Rect, app: &WizardApp) {
    let count = current_count(app).unwrap_or(default_count(app));
    let matching: Vec<&Preset> = presets_for(count).collect();

    if matching.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                format!("No presets for {count} panes."),
                style::error(),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  Press Esc to go back and pick a different count, or [E] to \
                 keep your existing layout.",
                style::help_text(),
            )),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    render_preset_list(frame, cols[0], &matching, app.focus);
    render_preset_preview(frame, cols[1], matching[app.focus.min(matching.len() - 1)]);
}

fn render_preset_list(frame: &mut Frame, area: Rect, matching: &[&Preset], focus: usize) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("Pick a {}-pane layout:", matching[0].cells),
        style::section_header(),
    )));
    lines.push(Line::from(""));
    for (i, preset) in matching.iter().enumerate() {
        let focused = i == focus;
        let marker = if focused { "▶ " } else { "  " };
        let label_style = if focused {
            style::option_selected()
        } else {
            style::option_idle()
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(marker.to_string(), label_style),
            Span::styled(preset.label.to_string(), label_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑/↓ to highlight · Enter to pick · Esc to change count",
        style::help_text(),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_preset_preview(frame: &mut Frame, area: Rect, preset: &Preset) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        preset.label.to_string(),
        style::section_header(),
    )));
    lines.push(Line::from(""));
    for row in &preset.ascii {
        lines.push(Line::from(Span::styled(
            row.clone(),
            style::value_idle(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        preset.description.to_string(),
        style::blurb(),
    )));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_pane_count_has_at_least_two_presets() {
        // 1-pane is the natural exception — there's only one geometry for
        // a single full-screen pane. Every other supported count should
        // give the user a real choice.
        for count in 2..=MAX_PANES {
            let n = presets_for(count).count();
            assert!(
                n >= 2,
                "pane count {count} only has {n} preset(s); need at least 2"
            );
        }
    }

    #[test]
    fn preset_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for p in all_presets() {
            assert!(seen.insert(&p.id), "duplicate preset id: {}", p.id);
        }
    }

    #[test]
    fn preset_grid_def_matches_cell_count() {
        for p in all_presets() {
            assert_eq!(
                p.grid_def.len(),
                p.cells,
                "preset {} declares {} cells but grid_def has {} entries",
                p.id,
                p.cells,
                p.grid_def.len()
            );
        }
    }
}
