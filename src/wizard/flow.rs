//! Page sequencing. Pure functions over `WizardState` — no I/O, no UI —
//! so the app loop can ask "what's next?" without baking the order into
//! the loop itself.
//!
//! The flow is linear today:
//!
//!   Welcome → Global → Layout → Assign → Widget(0..N) → Confirm
//!
//! Welcome appears on every run; on first-run it's an intro, on re-runs it
//! offers `[Resume]` when a `.wizard_state.toml` is present. Per-widget
//! pages are dynamic — one per entry in `state.assignments`.

#![allow(dead_code)] // consumed by app.rs once pages module lands.

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
        s if s.starts_with("widget-") => {
            s.strip_prefix("widget-")?.parse().ok().map(Page::Widget)
        }
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
        Page::Welcome => Some(Page::Global),
        Page::Global => Some(Page::Layout),
        Page::Layout => Some(Page::Assign),
        Page::Assign => match first_populated(state, 0) {
            Some(i) => Some(Page::Widget(i)),
            None => Some(Page::Confirm),
        },
        Page::Widget(i) => match first_populated(state, i + 1) {
            Some(j) => Some(Page::Widget(j)),
            None => Some(Page::Confirm),
        },
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
        Page::Welcome => None,
        Page::Global => Some(Page::Welcome),
        Page::Layout => Some(Page::Global),
        Page::Assign => Some(Page::Layout),
        Page::Widget(i) => match last_populated_before(state, *i) {
            Some(j) => Some(Page::Widget(j)),
            None => Some(Page::Assign),
        },
        Page::OAuthSetup { .. } => None,
        Page::AssignStack { .. } => None,
        Page::Confirm => match last_populated_before(state, state.assignments.len()) {
            Some(i) => Some(Page::Widget(i)),
            None => Some(Page::Assign),
        },
    }
}

/// Find the first populated cell at index `>= start`.
fn first_populated(state: &WizardState, start: usize) -> Option<usize> {
    state
        .assignments
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, a)| !a.kind.is_empty())
        .map(|(i, _)| i)
}

/// Find the largest populated cell index `< before`.
fn last_populated_before(state: &WizardState, before: usize) -> Option<usize> {
    state
        .assignments
        .iter()
        .enumerate()
        .take(before)
        .rev()
        .find(|(_, a)| !a.kind.is_empty())
        .map(|(i, _)| i)
}

/// 1-based step index for the progress header (`step 3 of 7`). Counts
/// only populated cells, so the progress reflects what the user will
/// actually walk through.
pub fn current_step(current: &Page, state: &WizardState) -> usize {
    let populated = populated_count(state);
    match current {
        Page::Welcome => 1,
        Page::Global => 2,
        Page::Layout => 3,
        Page::Assign => 4,
        Page::Widget(i) => 4 + populated_position(state, *i),
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

fn populated_count(state: &WizardState) -> usize {
    state.assignments.iter().filter(|a| !a.kind.is_empty()).count()
}

/// 1-based position of cell `idx` among the populated cells (i.e. the
/// nth populated cell). Used to derive widget-step numbers in the
/// progress bar.
fn populated_position(state: &WizardState, idx: usize) -> usize {
    let mut pos = 0;
    for (i, a) in state.assignments.iter().enumerate() {
        if a.kind.is_empty() {
            continue;
        }
        pos += 1;
        if i == idx {
            return pos;
        }
    }
    pos
}
