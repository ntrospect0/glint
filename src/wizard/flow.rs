// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

//! Page sequencing. Pure functions over `WizardState` — no I/O, no UI —
//! so the app loop can ask "what's next?" without baking the order into
//! the loop itself.
//!
//! Order: Welcome → Global → Layout → Assign → Widget(0..N) → Confirm.
//! Welcome appears on every run; on re-runs it offers `[Resume]` when a
//! `.wizard_state.toml` is present. Per-widget pages are dynamic — one
//! per entry in `state.assignments`.

#![allow(dead_code)]

use super::pages::Page;
use super::state::WizardState;

/// Page the wizard should land on when launched. Re-runs with a resume
/// buffer skip directly to the user's last visited page; otherwise we
/// start at Welcome.
pub fn start_page(state: &WizardState) -> Page {
    match state.last_page.as_deref() {
        Some(id) => page_from_id(id).unwrap_or(Page::Welcome),
        None => Page::Welcome,
    }
}

/// Resolve a stored page id back into a `Page`. Returns `None` for ids
/// that don't round-trip (e.g. a stale "widget-5" when the user has only
/// 3 widgets assigned now); the caller should fall back to Welcome.
fn page_from_id(id: &str) -> Option<Page> {
    match id {
        "welcome" => Some(Page::Welcome),
        "global" => Some(Page::Global),
        "layout" => Some(Page::Layout),
        "assign" => Some(Page::Assign),
        "confirm" => Some(Page::Confirm),
        s if s.starts_with("widget-") => s.strip_prefix("widget-")?.parse().ok().map(Page::Widget),
        _ => None,
    }
}

/// Next page in the linear flow, or `None` when we're past Confirm (the
/// app loop interprets `None` as "finalize and exit"). Empty cells in
/// `state.assignments` are skipped — the user marked them unassigned on
/// the Assign page; we shouldn't show a useless widget configuration
/// page for an empty cell.
pub fn next_page(current: &Page, state: &WizardState) -> Option<Page> {
    match current {
        // Manager is the front page, not part of the linear flow — it
        // transitions via PageAction::EnterProfileEdit, not Advance.
        Page::Manager => None,
        Page::Welcome => Some(Page::Global),
        Page::Global => Some(Page::Layout),
        Page::Layout => Some(Page::Assign),
        Page::Assign => Some(
            first_config_pos(state, 0)
                .map(ConfigPos::to_page)
                .unwrap_or(Page::Confirm),
        ),
        Page::Widget(i) => Some(
            next_config_pos(
                state,
                ConfigPos {
                    cell: *i,
                    child: None,
                },
            )
            .map(ConfigPos::to_page)
            .unwrap_or(Page::Confirm),
        ),
        Page::StackChild {
            cell_index,
            child_index,
        } => Some(
            next_config_pos(
                state,
                ConfigPos {
                    cell: *cell_index,
                    child: Some(*child_index),
                },
            )
            .map(ConfigPos::to_page)
            .unwrap_or(Page::Confirm),
        ),
        // OAuthSetup is out-of-band — it's never the "current" page in
        // the linear flow's forward sense; the app loop pushes/pops it
        // around the regular sequence via the history stack.
        Page::OAuthSetup { .. } => None,
        Page::AssignStack { .. } => None,
        Page::Confirm => None,
    }
}

/// Previous page in the linear flow, or `None` when there isn't one
/// (i.e. we're at Welcome). Used by the Back action as a fallback when
/// the visited-history stack is empty.
pub fn prev_page(current: &Page, state: &WizardState) -> Option<Page> {
    match current {
        Page::Manager => None,
        Page::Welcome => None,
        Page::Global => Some(Page::Welcome),
        Page::Layout => Some(Page::Global),
        Page::Assign => Some(Page::Layout),
        Page::Widget(i) => Some(
            prev_config_pos(
                state,
                ConfigPos {
                    cell: *i,
                    child: None,
                },
            )
            .map(ConfigPos::to_page)
            .unwrap_or(Page::Assign),
        ),
        Page::StackChild {
            cell_index,
            child_index,
        } => Some(
            prev_config_pos(
                state,
                ConfigPos {
                    cell: *cell_index,
                    child: Some(*child_index),
                },
            )
            .map(ConfigPos::to_page)
            .unwrap_or(Page::Assign),
        ),
        Page::OAuthSetup { .. } => None,
        Page::AssignStack { .. } => None,
        Page::Confirm => Some(
            last_config_pos(state)
                .map(ConfigPos::to_page)
                .unwrap_or(Page::Assign),
        ),
    }
}

/// A position in the per-widget walk: a single-widget cell, or a
/// specific child of a stack cell. Drives next/prev navigation and the
/// progress-step counter so stack children get the same per-page
/// treatment that single-widget cells get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigPos {
    cell: usize,
    child: Option<usize>,
}

impl ConfigPos {
    fn to_page(self) -> Page {
        match self.child {
            None => Page::Widget(self.cell),
            Some(k) => Page::StackChild {
                cell_index: self.cell,
                child_index: k,
            },
        }
    }
}

/// First configurable position at cell index `>= start`. A cell counts
/// when it has either a real `kind` (single widget) or non-empty
/// `stack_children` (stack — we yield its first child).
fn first_config_pos(state: &WizardState, start: usize) -> Option<ConfigPos> {
    for (i, a) in state.assignments.iter().enumerate().skip(start) {
        if !a.kind.is_empty() {
            return Some(ConfigPos {
                cell: i,
                child: None,
            });
        }
        if !a.stack_children.is_empty() {
            return Some(ConfigPos {
                cell: i,
                child: Some(0),
            });
        }
    }
    None
}

/// Next configurable position strictly after `from`. Walks remaining
/// stack children of the current cell, then moves on to subsequent
/// cells via [`first_config_pos`].
fn next_config_pos(state: &WizardState, from: ConfigPos) -> Option<ConfigPos> {
    if let (Some(k), Some(a)) = (from.child, state.assignments.get(from.cell)) {
        if k + 1 < a.stack_children.len() {
            return Some(ConfigPos {
                cell: from.cell,
                child: Some(k + 1),
            });
        }
    }
    first_config_pos(state, from.cell + 1)
}

/// Previous configurable position strictly before `from`. Mirrors
/// [`next_config_pos`]: walks earlier stack children of the current
/// cell first, then earlier cells (landing on their last child if
/// they're a stack).
fn prev_config_pos(state: &WizardState, from: ConfigPos) -> Option<ConfigPos> {
    if let (Some(k), _) = (from.child, state.assignments.get(from.cell)) {
        if k > 0 {
            return Some(ConfigPos {
                cell: from.cell,
                child: Some(k - 1),
            });
        }
    }
    last_config_pos_before(state, from.cell)
}

/// Last configurable position across the whole assignment list — used
/// by Confirm's Back to land on the final widget page.
fn last_config_pos(state: &WizardState) -> Option<ConfigPos> {
    last_config_pos_before(state, state.assignments.len())
}

/// Last configurable position at cell index `< before`. If the last
/// matching cell is a stack, returns its LAST child (so back-nav lands
/// on the deepest position the user actually walked through).
fn last_config_pos_before(state: &WizardState, before: usize) -> Option<ConfigPos> {
    for i in (0..before.min(state.assignments.len())).rev() {
        let a = &state.assignments[i];
        if !a.kind.is_empty() {
            return Some(ConfigPos {
                cell: i,
                child: None,
            });
        }
        if !a.stack_children.is_empty() {
            return Some(ConfigPos {
                cell: i,
                child: Some(a.stack_children.len() - 1),
            });
        }
    }
    None
}

/// 1-based step index for the progress header (`step 3 of 7`). Counts
/// only populated cells, so the progress reflects what the user will
/// actually walk through.
pub fn current_step(current: &Page, state: &WizardState) -> usize {
    let populated = populated_count(state);
    match current {
        Page::Manager => 0,
        Page::Welcome => 1,
        Page::Global => 2,
        Page::Layout => 3,
        Page::Assign => 4,
        Page::Widget(i) => {
            4 + position_of(
                state,
                ConfigPos {
                    cell: *i,
                    child: None,
                },
            )
        }
        Page::StackChild {
            cell_index,
            child_index,
        } => {
            4 + position_of(
                state,
                ConfigPos {
                    cell: *cell_index,
                    child: Some(*child_index),
                },
            )
        }
        // OAuthSetup overlays whatever widget page pushed it onto the
        // history stack; we can't recompute the originating widget
        // index cheaply, so report the last populated widget's step.
        Page::OAuthSetup { .. } => 4 + populated.max(1),
        // AssignStack overlays Assign — same step number.
        Page::AssignStack { .. } => 4,
        Page::Confirm => 5 + populated.max(1),
    }
}

/// Total step count for the progress header.
pub fn total_steps(state: &WizardState) -> usize {
    // Welcome + Global + Layout + Assign + N populated widgets + Confirm.
    // `max(1)` keeps the count sensible when assignments haven't been
    // filled in yet (we still know Confirm is coming).
    5 + populated_count(state).max(1)
}

/// Total number of configurable widget pages the wizard will walk
/// through after Assign — counts single-widget cells once and every
/// stack child individually, so the progress total reflects the full
/// per-widget walk.
fn populated_count(state: &WizardState) -> usize {
    state
        .assignments
        .iter()
        .map(|a| {
            if !a.kind.is_empty() {
                1
            } else if !a.stack_children.is_empty() {
                a.stack_children.len()
            } else {
                0
            }
        })
        .sum()
}

/// 1-based position of `target` in the per-widget walk — counts each
/// stack child individually, matching [`populated_count`].
fn position_of(state: &WizardState, target: ConfigPos) -> usize {
    let mut pos = 0;
    for (i, a) in state.assignments.iter().enumerate() {
        if !a.kind.is_empty() {
            pos += 1;
            if target.cell == i && target.child.is_none() {
                return pos;
            }
        } else if !a.stack_children.is_empty() {
            for k in 0..a.stack_children.len() {
                pos += 1;
                if target.cell == i && target.child == Some(k) {
                    return pos;
                }
            }
        }
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wizard::state::{CellAssignment, StackChild};

    fn cell(kind: &str) -> CellAssignment {
        CellAssignment {
            cell_index: 0,
            kind: kind.into(),
            instance: "main".into(),
            stack_children: Vec::new(),
        }
    }

    fn stack_cell(children: &[&str]) -> CellAssignment {
        CellAssignment {
            cell_index: 0,
            kind: String::new(),
            instance: "main".into(),
            stack_children: children
                .iter()
                .map(|k| StackChild {
                    kind: (*k).into(),
                    instance: "main".into(),
                })
                .collect(),
        }
    }

    /// Stack cells now route into a per-child walk instead of a
    /// single empty Widget page. Single-cell assignments still
    /// produce Widget(i); stack cells produce StackChild(i, k) for
    /// each child in order.
    #[test]
    fn next_page_walks_into_stack_children() {
        let mut state = WizardState::default();
        state.assignments.push(cell("clock"));
        state
            .assignments
            .push(stack_cell(&["news", "email", "notes"]));
        state.assignments.push(cell("calendar"));

        // Assign → first config = Widget(0) (clock).
        assert_eq!(next_page(&Page::Assign, &state), Some(Page::Widget(0)));
        // Clock → first stack child (cell 1, child 0 = news).
        assert_eq!(
            next_page(&Page::Widget(0), &state),
            Some(Page::StackChild {
                cell_index: 1,
                child_index: 0,
            })
        );
        // News → email (cell 1, child 1).
        assert_eq!(
            next_page(
                &Page::StackChild {
                    cell_index: 1,
                    child_index: 0,
                },
                &state
            ),
            Some(Page::StackChild {
                cell_index: 1,
                child_index: 1,
            })
        );
        // Email → notes.
        assert_eq!(
            next_page(
                &Page::StackChild {
                    cell_index: 1,
                    child_index: 1,
                },
                &state
            ),
            Some(Page::StackChild {
                cell_index: 1,
                child_index: 2,
            })
        );
        // Notes (last stack child) → calendar (cell 2).
        assert_eq!(
            next_page(
                &Page::StackChild {
                    cell_index: 1,
                    child_index: 2,
                },
                &state
            ),
            Some(Page::Widget(2))
        );
        // Calendar → Confirm.
        assert_eq!(next_page(&Page::Widget(2), &state), Some(Page::Confirm));
    }

    /// Esc/back from a stack child reverses the same walk — the user
    /// shouldn't get teleported past sibling children or to the
    /// parent cell's Assign page when there are earlier siblings to
    /// visit.
    #[test]
    fn prev_page_unwalks_through_stack_children() {
        let mut state = WizardState::default();
        state.assignments.push(cell("clock"));
        state.assignments.push(stack_cell(&["news", "email"]));

        // Calendar wasn't assigned, so Confirm's prev is the last
        // stack child of cell 1 (email).
        assert_eq!(
            prev_page(&Page::Confirm, &state),
            Some(Page::StackChild {
                cell_index: 1,
                child_index: 1,
            })
        );
        // Email → news (same stack, prev child).
        assert_eq!(
            prev_page(
                &Page::StackChild {
                    cell_index: 1,
                    child_index: 1,
                },
                &state
            ),
            Some(Page::StackChild {
                cell_index: 1,
                child_index: 0,
            })
        );
        // News (first child) → clock (cell 0).
        assert_eq!(
            prev_page(
                &Page::StackChild {
                    cell_index: 1,
                    child_index: 0,
                },
                &state
            ),
            Some(Page::Widget(0))
        );
        // Clock → Assign.
        assert_eq!(prev_page(&Page::Widget(0), &state), Some(Page::Assign));
    }

    /// populated_count counts each stack child individually so the
    /// "step N of M" progress reflects the actual page sequence.
    #[test]
    fn populated_count_includes_each_stack_child() {
        let mut state = WizardState::default();
        state.assignments.push(cell("clock"));
        state
            .assignments
            .push(stack_cell(&["news", "email", "notes"]));
        state.assignments.push(cell("calendar"));
        // 1 (clock) + 3 (stack children) + 1 (calendar) = 5.
        assert_eq!(populated_count(&state), 5);
    }
}
