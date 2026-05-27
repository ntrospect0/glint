# Stack widget — spec

A **stack** is a layout cell that contains up to 3 widgets, only one
visible at a time, with a tab strip in the title bar for switching.
Same screen real estate as a normal cell, three times the content.

This spec captures the design decisions made before implementation
begins; everything below is final unless a section is flagged "open."

---

## 1. Schema (`config.toml`)

A stacked cell is a `[[layout.cells]]` block that uses the **`widgets`
array field** instead of the scalar `widget` field. Backwards-compatible
with every existing layout — single-widget cells need no migration.

```toml
[[layout.cells]]
widgets = ["clock", "weather", "stocks"]
col = 0
row = 0
col_span = 1
row_span = 1
```

- `widgets` is a `Vec<String>` of widget ids (kind names or
  `kind@instance` forms — multi-instance support works naturally:
  `widgets = ["clock@home", "weather", "clock@work"]` is valid).
- **Max 3 elements** — config-load rejects 4+ with a clear error.
- **No gaps** — empty strings are silently dropped at parse time so
  the stack is always N contiguous widgets.
- **No nesting** — referencing another stack as a child is rejected
  at config-load.
- **Degrade to regular cell**: when a stack ends up with 1 widget
  after gap-stripping, it renders with no tab strip / rotation keys
  (identical to a non-stacked cell). The `widgets = [...]` form is
  preserved in config so the user can re-add widgets later without
  redoing the layout.

Existing `widget = "clock"` keeps working unchanged; presence of
`widgets = [...]` flips a cell into stack mode.

## 2. Global setting

One new key in `[global]`:

```toml
[global]
stack_hidden_poll_ratio = 3   # default; range 1..=N
```

Each non-visible widget in a stack has its poll interval multiplied
by this ratio. `1` = full rate (same cadence as the visible widget);
`3` (default) = three times slower; higher = even less frequent.
Applies globally across all stacks. A user who wants fresh-on-switch
sets it to `1`.

Implementation: at widget construction, a `WidgetCtx` flag indicates
whether the widget is a hidden stack child; if so, the widget's
configured poll interval is multiplied by the ratio before being used
to schedule fetches. Visible widgets in the same stack use their
unmodified interval.

## 3. Keybindings

When a stack-cell is focused:

- **`,`** — rotate to previous tab.
- **`.`** — rotate to next tab.
- **Shift+`<letter>`** for a child widget — switch the stack to show
  that widget *and* focus the cell. The shortcut dispatcher walks
  into stacks when resolving a letter; if the letter matches a hidden
  child, the stack rotates to make it visible first.

Other keys received by a focused stack cell flow through to the
currently-visible child widget unchanged. The stack consumes only the
two rotation keys.

The footer hint row and the tab strip both surface `, .` so the
rotation keys are discoverable.

## 4. Tab-strip rendering

Drawn in the cell's top border, replacing the single widget title.

**Full mode** (when total width fits the cell):
```
┌─ [• Clock] [Weather] [Stocks] ────────────────────────────┐
```

**Compact mode** (when full titles overflow):
```
┌─ [• C] [W] [S] ───────────────────────────────────────────┐
```

The renderer tries full mode first and falls back to compact when the
joined width exceeds the cell width. `•` precedes the active tab in
both modes. Active-tab styling uses `option_selected` (consistent with
the wizard's option lists); inactive tabs use the widget's normal
title style.

Multi-instance children render their full `kind@instance` form in
full mode (`[• clock@home] [clock@work]`) and the kind initial in
compact (`[• c] [c]`). The instance-collision case is rare; users
who hit it can rename via instance suffix.

## 5. Persistence

The active-tab index for each stack is **remembered across glint
restarts**. It's user-state, not config, so it lives in a separate
runtime-state file:

```
~/.config/glint/.runtime_state.toml
```

Shape:

```toml
version = 1

[[stack_state]]
cell_index = 2
active_tab = 1   # 0-indexed into the cell's `widgets` array
```

- File is written on tab change (debounced) and on graceful exit.
- Missing file or missing entry → stack defaults to tab 0.
- The leading dot (`.runtime_state.toml`) keeps it out of casual
  `ls` listings of the config dir, matching the `.wizard_state.toml`
  convention.

## 6. Wizard integration

**Assign page** gets a new option in the per-cell widget-kind picker:

```
( ) Clock
( ) Weather
( ) Stocks
…
( ) Stack — put up to 3 widgets in this slot
( ) (empty — skip this cell)
```

Picking **Stack** doesn't commit the cell; instead it pushes a new
page onto the wizard's history:

**`Page::AssignStack { cell_index }`** — walks the user through three
sequential slot pickers:

```
┌─ Configure stack for cell 2 ─────────────────────────────┐
│                                                          │
│ Slot 1 (always visible by default)                       │
│   (•) Clock                                              │
│   ( ) Weather                                            │
│   ( ) Stocks                                             │
│   ( ) (skip)                                             │
│                                                          │
│ Slot 2                                                   │
│   ( ) Clock                                              │
│   …                                                      │
│                                                          │
│ Slot 3                                                   │
│   ( ) Clock                                              │
│   …                                                      │
│                                                          │
│ [ Save & Next ]                                          │
└──────────────────────────────────────────────────────────┘
```

- Same field-navigation conventions as other wizard pages (Tab cycles
  slots, `,`/`.` are inert here, Enter on a slot row picks, Enter on
  `[ Save & Next ]` commits the stack and pops back to Assign).
- After commit, the cell's row on the Assign page summarises the
  stack: `Cell 2 — Stack: Clock + Weather + Stocks`.
- Empty slots are dropped at commit time (per §1's "no gaps" rule);
  if all three slots are skipped, the cell stays in its previous
  state (no-op).
- Esc returns to Assign without committing.

The OAuthSetup page is the existing precedent for sub-pages pushed
onto history; AssignStack reuses the same dispatch / on_enter / pop
pattern.

## 7. Implementation outline

New types:

- `WidgetCtx` gains `hidden_in_stack: bool` (used at widget
  construction to scale poll intervals).
- `StackWidget` — wraps `Vec<Box<dyn Widget>>` and an active-index;
  delegates `render` / `handle_key` to the active child;
  intercepts `.` / `,` for rotation; emits a tab-strip header.
- `Page::AssignStack { cell_index: usize }` — new wizard page.

Touched files (estimate):

- `src/layout.rs` (or wherever `[[layout.cells]]` deserialises):
  add `widgets: Option<Vec<String>>` alongside `widget`; reject
  conflicting both-present cases; reject 4+ entries; reject nested
  stacks.
- `src/widgets/registry.rs`: factory recognises `widgets` array and
  builds a `StackWidget` of children.
- New `src/widgets/stack.rs`: the `StackWidget` impl + tab-strip
  renderer.
- `src/app.rs`: shortcut dispatcher walks stacks when resolving
  `Shift+<letter>`; routes `,` / `.` to focused stack.
- `src/state/runtime.rs` (new): load/save for `.runtime_state.toml`.
- `src/wizard/pages/assign.rs`: "Stack" entry triggers
  `PageAction::PushPage(Page::AssignStack { cell_index })`.
- `src/wizard/pages/assign_stack.rs` (new): the sub-page.
- `src/wizard/finalize.rs`: emit `widgets = [...]` when the cell's
  state holds 2+ widgets.
- `src/wizard/hydrate.rs`: recognise `widgets` field on hydrate.

Tests:

- Schema round-trip (single-widget stacks degrade, gap-stripping,
  4+ rejection, nested rejection).
- StackWidget rotation (`.` / `,` wrap correctly).
- Shortcut walks into hidden child.
- Tab-strip full → compact fallback at given widths.
- Hidden poll-interval scaling.
- AssignStack wizard sub-page (commit + cancel paths).
- Runtime-state file save/load round-trip + missing-file fallback.

## 8. Open questions

None — every decision in §1–§7 has a chosen value. Implementation
can proceed straight from this spec.

## 9. Future work (out of scope for v1)

- Mouse: click a tab to switch.
- Animated transitions (probably never — TUIs prefer instant).
- Per-stack `default_visible` override (covered by persistence; only
  needed if someone explicitly wants "always start on slot 2").
- Stacks of 4+ (raise the cap if real demand surfaces).
- Stack inside stack (deliberately deferred; revisit only if
  someone has a real use case).
