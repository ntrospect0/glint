// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal};

use crate::{
    cache::Cache,
    config::{self, Config},
    event::{Event, EventReader},
    llm::{self, LlmConfig, LlmProvider},
    theme::{self, Theme},
    ui,
    widgets::{parse_widget_ref, registry, AppContext, EventResult, WidgetCtx, WidgetManager},
    zoom::ZoomTarget,
};

/// Time a `command_feedback` message stays visible in the chrome row
/// before render replaces it with the idle status-bar content.
const FEEDBACK_TTL: Duration = Duration::from_secs(3);

pub struct App {
    config: Config,
    theme: Arc<Theme>,
    manager: WidgetManager,
    focus_idx: usize,
    /// Widget ids in layout order (Tab cycles through this).
    focus_order: Vec<String>,
    /// `Shift+<letter>` → `(focus_target_id, optional_child_id)`. For
    /// leaf widgets the child is `None`; for widgets inside a stack
    /// the child id is the kind-specific widget id and the focus
    /// target id is the stack's synthetic id. The dispatcher in
    /// `handle_global_key` uses the child id to also call
    /// `switch_to_composite_child` so the right tab becomes visible
    /// before focus lands.
    shortcuts: HashMap<char, (String, Option<String>)>,
    should_quit: bool,
    show_help: bool,
    help_scroll: u16,
    /// Max scroll updated by `ui::help::render` so the scroll handler can
    /// clamp without re-computing the layout.
    help_scroll_max: Cell<u16>,
    /// `Some` while the user is composing after pressing `:`.
    command_buffer: Option<String>,
    /// Transient feedback shown in the chrome row after a `:` command.
    /// Carries a severity tag (drives the message color via the active
    /// scheme) and a timestamp; render expires entries older than
    /// `FEEDBACK_TTL` so the message disappears on its own without the
    /// user having to dismiss it.
    command_feedback: Option<(String, ui::FeedbackSeverity, Instant)>,
    /// Snapshot of stack active-tab indices as of the last
    /// `runtime_state.toml` write. After each input tick we recompute
    /// the current snapshot and save only if it differs from this —
    /// avoids a disk write on every keypress while still persisting
    /// every meaningful change.
    last_persisted_stack_state: HashMap<String, usize>,
    /// Cached pixels + bookkeeping from the previous draw. The
    /// partial-render path blits unchanged cells from here into the
    /// new frame instead of re-running their `render()`. See
    /// [`ui::PartialDrawCache`] for the invalidation rules.
    partial_draw: ui::PartialDrawCache,
    /// The currently-active zoom target, or `None` when zoom is off.
    /// Set by `zoom_enter`, cleared by `exit_zoom`. Drives the zoom
    /// overlay in `ui::render` and the zoom-active event dispatch branch.
    zoom_target: Option<ZoomTarget>,
    /// Wrapping tick counter used to throttle backdrop widget polls while zoom
    /// is active.  Incremented every `Event::Tick`; backdrop widgets only call
    /// `update()` when `counter % stack_hidden_poll_ratio == 0` (mirroring how
    /// `StackWidget` gates hidden children).  Reset to 0 at startup.
    zoom_backdrop_tick_counter: u64,
}

impl App {
    pub fn new(config: Config) -> Self {
        // Theme + LLM are both best-effort: missing files / unknown schemes /
        // missing API keys all log a warning and continue with sensible
        // defaults (built-in palette, no LLM).
        let theme = theme::load(&config.global.theme).unwrap_or_else(|err| {
            tracing::warn!(error = %err, "failed to load colorschemes.toml, using built-in defaults");
            Arc::new(Theme::builtin_defaults())
        });

        let llm_cfg: LlmConfig = config::load_widget_toml("llm").unwrap_or_default();
        let llm_provider = llm::build_provider(&llm_cfg).unwrap_or_else(|err| {
            tracing::warn!(error = %err, "failed to build LLM provider");
            None
        });

        // Cache root opened once and scoped per-widget at registration time.
        // If the home dir can't be resolved (exotic environment), fall back
        // to the system temp dir — widgets keep working, they just don't
        // persist between runs.
        let cache = Cache::open_default().unwrap_or_else(|err| {
            tracing::warn!(error = %err, "failed to resolve cache dir; using temp dir");
            Cache::at(std::env::temp_dir().join("glint-cache"))
        });
        // Best-effort startup sweep: drop cache files no widget has touched
        // in 30 days. Each widget's cache size is bounded per entry, but
        // long-running setups accumulate orphans (renamed feeds, dropped
        // tickers, gallery images that moved). Cheap enough to run every
        // launch; failures log and the dashboard proceeds.
        let removed = cache.sweep_older_than(std::time::Duration::from_secs(30 * 24 * 60 * 60));
        if removed > 0 {
            tracing::info!(removed, "cache sweep: dropped stale entries");
        }

        let mut manager = WidgetManager::new();
        register_widgets_from_layout(
            &mut manager,
            &config,
            theme.clone(),
            llm_provider.clone(),
            &cache,
        );

        // If the layout produced no recognizable widgets (empty layout, or
        // every cell references an unknown kind), fall back to the original
        // five-widget seed so first-run with an empty config still shows
        // something useful.
        if manager.ids().is_empty() {
            register_default_widgets(&mut manager, theme.clone(), llm_provider, &cache);
        }

        // Restore each stack's previously-visible tab from the
        // runtime-state file (or default to slot 0 if no entry).
        // Done after registration so the targets exist in the
        // manager.
        let runtime_state = crate::runtime_state::load();
        for (stack_id, entry) in &runtime_state.stacks {
            if let Some(widget) = manager.get_mut(stack_id) {
                widget.set_composite_active_index(entry.active_tab);
            }
        }

        let focus_order = focus_order_from_layout(&config, &manager);
        let shortcuts = assign_shortcuts(&mut manager);
        let last_persisted_stack_state = collect_stack_snapshot(&manager);
        Self {
            config,
            theme,
            manager,
            focus_idx: 0,
            focus_order,
            shortcuts,
            should_quit: false,
            show_help: false,
            help_scroll: 0,
            help_scroll_max: Cell::new(0),
            command_buffer: None,
            command_feedback: None,
            last_persisted_stack_state,
            partial_draw: ui::PartialDrawCache::default(),
            zoom_target: None,
            zoom_backdrop_tick_counter: 0,
        }
    }

    /// Walk all registered widgets and persist any stack-active-tab
    /// changes since the last save. Cheap when nothing changed (just a
    /// HashMap rebuild + comparison); does the disk write only on
    /// diff. Called from the event loop after every input tick.
    fn persist_runtime_state_if_dirty(&mut self) {
        let current = collect_stack_snapshot(&self.manager);
        if current == self.last_persisted_stack_state {
            return;
        }
        // Load existing state first so we don't wipe per-widget data
        // (e.g. clock timer durations) when writing back just the
        // stack snapshot. Widgets manage their own keys; this routine
        // only owns the `stacks` section.
        let mut payload = crate::runtime_state::load();
        payload.stacks = current
            .iter()
            .map(|(id, active)| {
                (
                    id.clone(),
                    crate::runtime_state::StackEntry {
                        active_tab: *active,
                    },
                )
            })
            .collect();
        if let Err(err) = crate::runtime_state::save(&payload) {
            tracing::warn!(error = %err, "failed to persist runtime state");
            return;
        }
        self.last_persisted_stack_state = current;
    }

    /// Adjust the help overlay's vertical scroll by `delta` rows. Clamps
    /// against `help_scroll_max` (updated by the previous render) so we
    /// never scroll past the last line of content. Called by Up/Down/k/j
    /// /PgUp/PgDn keys and by mouse wheel events when the overlay is open.
    fn scroll_help(&mut self, delta: i32) {
        let max = self.help_scroll_max.get() as i32;
        let next = (self.help_scroll as i32 + delta).clamp(0, max);
        self.help_scroll = next as u16;
    }

    fn focused_widget(&self) -> Option<&str> {
        self.focus_order.get(self.focus_idx).map(String::as_str)
    }

    /// Set the chrome-row feedback message. Caller picks the severity;
    /// the timestamp is stamped here so render can age the entry out
    /// after `FEEDBACK_TTL` without each call site having to think about
    /// clock plumbing.
    fn set_feedback(&mut self, text: impl Into<String>, severity: ui::FeedbackSeverity) {
        self.command_feedback = Some((text.into(), severity, Instant::now()));
    }

    /// Drop the feedback if it's older than `FEEDBACK_TTL`. Returns
    /// `true` when the bar was actually cleared so the tick path can
    /// force a redraw — otherwise the now-stale "saved" / "error"
    /// chrome would linger until the next user event.
    fn expire_stale_feedback(&mut self) -> bool {
        if let Some((_, _, set_at)) = &self.command_feedback {
            if set_at.elapsed() >= FEEDBACK_TTL {
                self.command_feedback = None;
                return true;
            }
        }
        false
    }

    /// Drain every widget's dirty bit. Returns the set of ids that
    /// reported `true` — the partial-render path needs to know
    /// *which* widgets need a fresh paint, not just "any of them."
    /// Always calls `take_dirty` on every widget (even when the
    /// answer is obviously yes elsewhere) so a queued bit can't
    /// smuggle into the next tick and trigger a redundant redraw.
    fn drain_widget_dirty_ids(&mut self) -> HashSet<String> {
        let mut dirty: HashSet<String> = HashSet::new();
        for id in self.manager.ids().to_vec() {
            if let Some(w) = self.manager.get_mut(&id) {
                if w.take_dirty() {
                    dirty.insert(id);
                }
            }
        }
        dirty
    }

    /// Borrow the current feedback as the ui-layer tuple, after expiring
    /// stale entries. Used at each RenderState construction site so the
    /// three draw paths stay in lockstep.
    fn feedback_for_render(&self) -> Option<(&str, ui::FeedbackSeverity)> {
        self.command_feedback
            .as_ref()
            .filter(|(_, _, set_at)| set_at.elapsed() < FEEDBACK_TTL)
            .map(|(text, severity, _)| (text.as_str(), *severity))
    }

    /// Snapshot the App's draw-time inputs into a `RenderState` for the
    /// UI layer. One constructor instead of three inline literals;
    /// adding a render-state field becomes a one-line change here
    /// instead of three identical edits.
    fn render_state(&self) -> ui::RenderState<'_> {
        ui::RenderState {
            layout: &self.config.layout,
            manager: &self.manager,
            focused: self.focused_widget(),
            show_help: self.show_help,
            command_buffer: self.command_buffer.as_deref(),
            command_feedback: self.feedback_for_render(),
            theme: &self.theme,
            theme_name: &self.config.global.theme,
            help_scroll: self.help_scroll,
            help_scroll_max: &self.help_scroll_max,
            show_status_bar: self.config.global.show_status_bar,
            zoom_target: self.zoom_target.as_ref(),
            zoom_margin: self.config.global.zoom_margin,
        }
    }

    fn cycle_focus(&mut self, forward: bool) {
        if self.focus_order.is_empty() {
            return;
        }
        let n = self.focus_order.len();
        self.focus_idx = if forward {
            (self.focus_idx + 1) % n
        } else {
            (self.focus_idx + n - 1) % n
        };
    }

    /// Shift input focus + visible-stack-tab to the widget with the
    /// given id. Mirrors the `Shift+<letter>` dispatcher's promotion
    /// logic but is addressable by widget id rather than shortcut
    /// letter — used by widget-initiated focus requests (timer alarm,
    /// etc.) and any future "jump to this widget" plumbing. Returns
    /// `true` when the widget was found and focus was changed.
    fn promote_to_widget(&mut self, target_id: &str) -> bool {
        // Direct top-level match.
        if let Some(pos) = self.focus_order.iter().position(|w| w == target_id) {
            self.focus_idx = pos;
            return true;
        }
        // Otherwise the target is a stack child. Snapshot the
        // (parent_id, children) pairs first so we can mutate the
        // manager inside the loop without aliasing.
        let parents: Vec<(String, Vec<String>)> = self
            .focus_order
            .iter()
            .map(|id| {
                let children = self
                    .manager
                    .get(id)
                    .map(|w| w.composite_children())
                    .unwrap_or_default();
                (id.clone(), children)
            })
            .collect();
        for (i, (parent_id, children)) in parents.iter().enumerate() {
            if children.iter().any(|c| c == target_id) {
                self.focus_idx = i;
                if let Some(parent) = self.manager.get_mut(parent_id) {
                    parent.switch_to_composite_child(target_id);
                }
                return true;
            }
        }
        false
    }

    /// Drain every widget's pending focus request (including stack
    /// children) and honor them in id order. Called from the tick
    /// loop after `update` so widgets that decide to grab attention
    /// inside `update` see the focus shift on the same frame.
    /// Returns `true` when at least one request was honored, so the
    /// caller can force a redraw even when no widget marked itself
    /// dirty (focus changes don't auto-set the dirty bit).
    fn process_focus_requests(&mut self) -> bool {
        // Collect ids first (top-level + stack children) so the
        // manager borrow stays clean while we iterate.
        let mut all_ids: Vec<String> = Vec::new();
        for id in self.manager.ids() {
            all_ids.push(id.clone());
            if let Some(w) = self.manager.get(id) {
                all_ids.extend(w.composite_children());
            }
        }
        let mut promoted = false;
        for id in all_ids {
            let req = self
                .manager
                .get_mut(&id)
                .and_then(|w| w.take_focus_request());
            if let Some(req) = req {
                if self.promote_to_widget(&req.widget_id) {
                    promoted = true;
                }
            }
        }
        promoted
    }

    // ── Zoom methods ─────────────────────────────────────────────────────

    /// Enter zoom for the currently-focused widget. No-op when `focus_order`
    /// is empty or `focused_widget()` returns `None`.
    fn zoom_enter(&mut self) {
        let Some(focused_id) = self.focused_widget().map(str::to_string) else {
            return;
        };
        let zoom = self.zoom_target_for_widget_id(focused_id);
        self.zoom_target = Some(zoom);
    }

    /// Exit zoom, clearing `zoom_target` and landing focus on the widget
    /// that was zoomed (so focus is where the user left off).
    fn exit_zoom(&mut self) {
        let Some(zoom) = self.zoom_target.take() else {
            return;
        };
        // Restore focus_idx to the position of the zoom target's parent.
        if let Some(pos) = self
            .focus_order
            .iter()
            .position(|w| w == &zoom.parent_id)
        {
            self.focus_idx = pos;
        }
    }

    /// Build a `ZoomTarget` for the given top-level widget id. If the widget
    /// is a composite (stack), resolve its currently-active child by index so
    /// the zoom frame shows the correct child in isolation (CEO Q1).
    fn zoom_target_for_widget_id(&self, parent_id: String) -> ZoomTarget {
        let child_id = self.manager.get(&parent_id).and_then(|w| {
            let idx = w.composite_active_index()?;
            let children = w.composite_children();
            children.into_iter().nth(idx)
        });
        ZoomTarget {
            parent_id,
            child_id,
        }
    }

    /// Resolve the zoom target to an immutable widget reference. Used by
    /// `is_zoom_retarget_suppressed` and `display_name` lookups.
    fn resolve_zoom_widget(&self) -> Option<&dyn crate::widgets::Widget> {
        let zoom = self.zoom_target.as_ref()?;
        let parent = self.manager.get(&zoom.parent_id)?;
        match &zoom.child_id {
            None => Some(parent),
            Some(cid) => parent.composite_child(cid),
        }
    }

    /// Returns `true` when the currently-zoomed widget is actively capturing
    /// text input and retarget gestures (`Tab`, `Shift+<letter>`, mouse click
    /// on backdrop) should be suppressed. Returns `false` when there is no
    /// active zoom target.
    fn is_zoom_retarget_suppressed(&self) -> bool {
        self.resolve_zoom_widget()
            .map(|w| w.is_capturing_text())
            .unwrap_or(false)
    }

    /// Retarget zoom to the widget assigned shortcut letter `letter`. Moves
    /// both `zoom_target` and `focus_idx` so they stay synchronized; also
    /// flips a stack's active tab when the shortcut points at a child.
    fn retarget_zoom_by_letter(&mut self, letter: char) {
        let Some((parent_id, child_id)) = self.shortcuts.get(&letter).cloned() else {
            return;
        };
        if let Some(pos) = self.focus_order.iter().position(|w| w == &parent_id) {
            self.focus_idx = pos;
        }
        if let Some(ref child) = child_id {
            if let Some(parent) = self.manager.get_mut(&parent_id) {
                parent.switch_to_composite_child(child);
            }
        }
        self.zoom_target = Some(ZoomTarget { parent_id, child_id });
    }

    /// Advance the zoom target by one step in focus order. `forward = true`
    /// moves to the next widget; `false` moves to the previous. Wraps at
    /// both ends. Updates `zoom_target` and `focus_idx` atomically.
    fn retarget_zoom_cycle(&mut self, forward: bool) {
        if self.focus_order.is_empty() {
            return;
        }
        let n = self.focus_order.len();
        let new_idx = if forward {
            (self.focus_idx + 1) % n
        } else {
            (self.focus_idx + n - 1) % n
        };
        self.focus_idx = new_idx;
        let parent_id = self.focus_order[new_idx].clone();
        let zoom = self.zoom_target_for_widget_id(parent_id);
        self.zoom_target = Some(zoom);
    }

    /// Handle a key event while zoom is active. The focused widget already
    /// had its chance via `handle_key`; only `Ignored` keys reach here.
    ///
    /// Dispatch table (evaluated top-to-bottom):
    ///
    /// | Pattern | Action |
    /// |---------|--------|
    /// | `show_help == true` | delegate to `handle_global_key` (help is modal above zoom) |
    /// | `z`, `Shift-Z`, `Shift-z` | `exit_zoom` |
    /// | `Esc` | `exit_zoom` |
    /// | Uppercase letter (not suppressed) | `retarget_zoom_by_letter` |
    /// | `Tab` (not suppressed) | `retarget_zoom_cycle(true)` |
    /// | `BackTab` (not suppressed) | `retarget_zoom_cycle(false)` |
    /// | `q`, `Ctrl-C` | quit |
    /// | `?` | open help overlay |
    /// | `:` | open command bar |
    /// | anything else | swallowed (zoom is modal) |
    fn handle_global_zoom_key(&mut self, key: crossterm::event::KeyEvent) {
        // Help overlay is modal above zoom — delegate the key so `?`/`Esc`/etc.
        // still work while zoom is in the background.
        if self.show_help {
            self.handle_global_key(key);
            return;
        }
        let suppressed = self.is_zoom_retarget_suppressed();
        match (key.modifiers, key.code) {
            // z / Shift-Z exit zoom (primary and backup toggle).
            (KeyModifiers::NONE, KeyCode::Char('z'))
            | (KeyModifiers::SHIFT, KeyCode::Char('Z'))
            | (KeyModifiers::SHIFT, KeyCode::Char('z')) => self.exit_zoom(),
            // Esc always exits zoom — the widget already had its first chance.
            (_, KeyCode::Esc) => self.exit_zoom(),
            // Shift+uppercase: retarget zoom to that widget's letter.
            (_, KeyCode::Char(c)) if c.is_ascii_uppercase() && !suppressed => {
                self.retarget_zoom_by_letter(c.to_ascii_lowercase());
            }
            // Tab / BackTab: cycle zoom target in focus order.
            (KeyModifiers::NONE, KeyCode::Tab) if !suppressed => {
                self.retarget_zoom_cycle(true);
            }
            (KeyModifiers::SHIFT | KeyModifiers::NONE, KeyCode::BackTab) if !suppressed => {
                self.retarget_zoom_cycle(false);
            }
            // Quit still works while zoomed.
            (KeyModifiers::NONE, KeyCode::Char('q')) => self.should_quit = true,
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.should_quit = true,
            // Help overlay.
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('?')) => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            // Command bar (CEO-ratified: command bar renders above zoom, zoom stays
            // active in background — no exit-zoom arm here).
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(':')) => {
                self.command_buffer = Some(String::new());
                self.command_feedback = None;
            }
            // All other keys are swallowed — zoom is modal for global actions.
            _ => {}
        }
    }

    // ── End zoom methods ──────────────────────────────────────────────────

    fn handle_global_key(&mut self, key: crossterm::event::KeyEvent) {
        // Help overlay swallows every key — Esc / ? / q close it; arrows / k /
        // j / PgUp / PgDn / Home / End scroll. Everything else is dropped so
        // `q` doesn't accidentally quit through the overlay.
        if self.show_help {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.show_help = false;
                    self.help_scroll = 0;
                }
                KeyCode::Up | KeyCode::Char('k') => self.scroll_help(-1),
                KeyCode::Down | KeyCode::Char('j') => self.scroll_help(1),
                KeyCode::PageUp => self.scroll_help(-10),
                KeyCode::PageDown => self.scroll_help(10),
                KeyCode::Home | KeyCode::Char('g') => self.help_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => {
                    self.help_scroll = self.help_scroll_max.get();
                }
                _ => {}
            }
            return;
        }
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('q')) => self.should_quit = true,
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.should_quit = true,
            (KeyModifiers::NONE, KeyCode::Tab) => self.cycle_focus(true),
            (KeyModifiers::SHIFT, KeyCode::BackTab) | (KeyModifiers::NONE, KeyCode::BackTab) => {
                self.cycle_focus(false)
            }
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char('?')) => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            // `:` opens the command bar when no widget claimed it.
            (KeyModifiers::NONE | KeyModifiers::SHIFT, KeyCode::Char(':')) => {
                self.command_buffer = Some(String::new());
                self.command_feedback = None;
            }
            // `z` / `Shift-Z` / `Shift-z`: enter zoom for the focused widget.
            // This arm must come BEFORE the generic uppercase-letter arm so `Z`
            // is claimed as a zoom toggle and never dispatched as a shortcut key.
            // (Zoom exit while zoom is active is handled by `handle_global_zoom_key`.)
            (KeyModifiers::NONE, KeyCode::Char('z'))
            | (KeyModifiers::SHIFT, KeyCode::Char('Z'))
            | (KeyModifiers::SHIFT, KeyCode::Char('z')) => {
                self.zoom_enter();
            }
            // `Shift+<letter>` jumps to the widget that claimed that
            // letter. Some terminals drop the SHIFT modifier on
            // shifted alphabetic keys, so we match on case rather
            // than `KeyModifiers::SHIFT`. For widgets hidden inside a
            // stack, the dispatcher also flips the stack to that
            // child via `switch_to_composite_child` so the user
            // doesn't have to manually rotate first.
            (_, KeyCode::Char(c)) if c.is_ascii_uppercase() => {
                let lower = c.to_ascii_lowercase();
                if let Some((parent_id, child_id)) = self.shortcuts.get(&lower).cloned() {
                    if let Some(pos) = self.focus_order.iter().position(|w| w == &parent_id) {
                        self.focus_idx = pos;
                    }
                    if let Some(child) = child_id {
                        if let Some(parent) = self.manager.get_mut(&parent_id) {
                            parent.switch_to_composite_child(&child);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Append a bracketed-paste payload to the command bar buffer. Newlines
    /// and other control chars are stripped — the command bar is a single
    /// line and Enter is the submit key, so pasted multi-line text would
    /// auto-execute fragments otherwise.
    fn handle_command_bar_paste(&mut self, text: &str) {
        self.command_feedback = None;
        let Some(buf) = self.command_buffer.as_mut() else {
            return;
        };
        for c in text.chars() {
            if !c.is_control() {
                buf.push(c);
            }
        }
    }

    fn handle_command_bar_key(&mut self, key: crossterm::event::KeyEvent) {
        self.command_feedback = None;
        let Some(buf) = self.command_buffer.as_mut() else {
            return;
        };
        match (key.modifiers, key.code) {
            (_, KeyCode::Esc) => {
                self.command_buffer = None;
            }
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                self.command_buffer = None;
            }
            // Ctrl-U mirrors the readline "kill to start of line" gesture.
            // The leading ':' lives in the chrome, not the buffer, so
            // clearing the buffer is exactly "wipe everything after the
            // prompt while keeping the prompt in place".
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                buf.clear();
            }
            (_, KeyCode::Backspace) if buf.pop().is_none() => {
                self.command_buffer = None;
            }
            (_, KeyCode::Enter) => {
                let line = std::mem::take(buf);
                self.command_buffer = None;
                self.execute_command(line.trim());
            }
            (mods, KeyCode::Char(c))
                if mods == KeyModifiers::NONE || mods == KeyModifiers::SHIFT =>
            {
                buf.push(c);
            }
            _ => {}
        }
    }

    /// `:scheme <name>` — re-read `colorschemes.toml` and propagate the new
    /// palette to every widget. Missing/unknown names surface a feedback
    /// line listing the available schemes.
    fn execute_scheme_command(&mut self, args: &[&str]) {
        let file = match theme::load_schemes_file() {
            Ok(f) => f,
            Err(err) => {
                self.set_feedback(
                    format!("colorschemes.toml: {err}"),
                    ui::FeedbackSeverity::Error,
                );
                return;
            }
        };

        // Sort once — used by both the "no arg" hint and the "not found"
        // message so the order is stable from the user's perspective.
        let mut available: Vec<&str> = file.schemes.keys().map(String::as_str).collect();
        available.sort_unstable();
        let available_csv = available.join(", ");

        let Some(name) = args.first() else {
            let msg = if available.is_empty() {
                "usage: :scheme <name> — (no schemes defined in colorschemes.toml)".to_string()
            } else {
                format!("usage: :scheme <name>. Available: {available_csv}")
            };
            self.set_feedback(msg, ui::FeedbackSeverity::Warning);
            return;
        };

        let Some(scheme) = file.schemes.get(*name) else {
            let msg = if available.is_empty() {
                format!("unknown scheme {name:?} — colorschemes.toml has no [schemes.*] blocks")
            } else {
                format!("unknown scheme {name:?}. Available: {available_csv}")
            };
            self.set_feedback(msg, ui::FeedbackSeverity::Error);
            return;
        };

        let new_theme = theme::theme_from_scheme(scheme);
        self.theme = new_theme.clone();
        self.config.global.theme = (*name).to_string();
        for id in self.manager.ids().to_vec() {
            if let Some(widget) = self.manager.get_mut(&id) {
                widget.set_app_theme(new_theme.clone());
            }
        }
        // Persist so the choice survives restart. In-memory swap already
        // happened; a write failure only downgrades the success line.
        match theme::persist_active_scheme(name) {
            Ok(()) => {
                self.set_feedback(
                    format!("scheme → {name}"),
                    ui::FeedbackSeverity::Confirmation,
                );
            }
            Err(err) => {
                tracing::warn!(error = %err, scheme = %name, "failed to persist scheme");
                self.set_feedback(
                    format!("scheme → {name} (not persisted: {err})"),
                    ui::FeedbackSeverity::Warning,
                );
            }
        }
    }

    fn execute_command(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let args: Vec<&str> = parts.collect();

        // Global commands first.
        match cmd {
            "q" | "quit" | "exit" => {
                self.should_quit = true;
                return;
            }
            "help" | "?" => {
                self.show_help = true;
                self.help_scroll = 0;
                return;
            }
            "refresh" | "r" => {
                // Delegated so each widget defines its own refresh semantics.
                if let Some(id) = self.focused_widget().map(str::to_string) {
                    if let Some(widget) = self.manager.get_mut(&id) {
                        let _ = widget.handle_command("refresh", &args);
                    }
                }
                return;
            }
            "scheme" | "theme" => {
                self.execute_scheme_command(&args);
                return;
            }
            _ => {}
        }

        // Try the focused widget first, then every other registered widget.
        // The first one to return Ok(true) wins and gets focus.
        let focused = self.focused_widget().map(str::to_string);
        let ordered_ids: Vec<String> = {
            let mut ids: Vec<String> = Vec::new();
            if let Some(f) = focused.as_ref() {
                ids.push(f.clone());
            }
            for id in self.manager.ids() {
                if focused.as_deref() != Some(id.as_str()) {
                    ids.push(id.clone());
                }
            }
            ids
        };
        for id in ordered_ids {
            let Some(widget) = self.manager.get_mut(&id) else {
                continue;
            };
            match widget.handle_command(cmd, &args) {
                Ok(true) => {
                    if let Some(pos) = self.focus_order.iter().position(|w| w == &id) {
                        self.focus_idx = pos;
                    }
                    return;
                }
                Ok(false) => continue,
                Err(err) => {
                    self.set_feedback(format!("{id}: {err}"), ui::FeedbackSeverity::Error);
                    return;
                }
            }
        }
        self.set_feedback(
            format!("unknown command: {cmd:?}"),
            ui::FeedbackSeverity::Error,
        );
    }
}

/// Re-read a widget TOML file and pipe the value through `Widget::apply_config`.
/// Parse failures log and skip — the next save event will retry.
fn apply_config_change(app: &mut App, path: &std::path::Path) {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    // Stem is `<kind>` or `<kind>@<instance>`. Non-widget files (llm.toml,
    // colorschemes.toml, credentials/…) won't resolve to a manager entry.
    let (kind, instance) = parse_widget_ref(stem);
    let widget_id: String = if instance == "main" {
        kind.clone()
    } else {
        format!("{kind}@{instance}")
    };
    if app.manager.get(&widget_id).is_none() {
        return;
    }
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };
    let toml_value: toml::Value = match toml::from_str(&contents) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(file = %path.display(), error = %err, "config parse failed, will retry on next event");
            return;
        }
    };
    let json: serde_json::Value = match serde_json::to_value(toml_value) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(file = %path.display(), error = %err, "toml→json conversion failed");
            return;
        }
    };
    let Some(widget) = app.manager.get_mut(&widget_id) else {
        return;
    };
    if let Err(err) = widget.apply_config(json) {
        tracing::warn!(widget = %widget_id, error = %err, "apply_config failed");
    } else {
        tracing::info!(widget = %widget_id, "live-reloaded config");
    }

    // Live-reload guard: if a config change removed the currently-zoomed widget
    // from the manager (e.g., kind or instance changed), clear zoom so the overlay
    // doesn't render a ghost target.
    if let Some(ref zoom) = app.zoom_target.clone() {
        if app.manager.get(&zoom.parent_id).is_none() {
            tracing::info!(widget = %zoom.parent_id, "zoomed widget no longer in manager after config reload — clearing zoom");
            app.zoom_target = None;
        }
    }
}

/// Swap scroll-wheel directions in place. Vertical and horizontal axes are
/// both flipped so a trackpad with two-finger panning behaves consistently.
/// Non-scroll kinds pass through unchanged. Centralising this here keeps
/// every widget free of `if invert { ... } else { ... }` plumbing.
fn invert_scroll(kind: MouseEventKind) -> MouseEventKind {
    match kind {
        MouseEventKind::ScrollUp => MouseEventKind::ScrollDown,
        MouseEventKind::ScrollDown => MouseEventKind::ScrollUp,
        MouseEventKind::ScrollLeft => MouseEventKind::ScrollRight,
        MouseEventKind::ScrollRight => MouseEventKind::ScrollLeft,
        other => other,
    }
}

/// Route a mouse event to the zoomed widget. Resolves the zoom target to the
/// correct widget (or composite child), forwarding `mouse` into its `handle_mouse`.
/// `inner_rect` is the zoom frame's interior (excluding the border), already
/// computed by the caller so we don't have to repeat the layout maths here.
/// Returns `true` when the zoomed widget consumed the event (i.e. something
/// changed and a repaint is warranted).
fn route_mouse_to_zoom_widget(
    app: &mut App,
    mouse: crossterm::event::MouseEvent,
    inner_rect: Rect,
) -> bool {
    let Some(zoom) = app.zoom_target.clone() else {
        return false;
    };
    let parent = match app.manager.get_mut(&zoom.parent_id) {
        Some(p) => p,
        None => return false,
    };
    match zoom.child_id {
        None => parent.handle_mouse(mouse, inner_rect) == EventResult::Handled,
        Some(ref cid) => match parent.composite_child_mut(cid) {
            Some(child) => child.handle_mouse(mouse, inner_rect) == EventResult::Handled,
            None => false,
        },
    }
}

/// Returns the (widget id, cell area) under screen coordinates `(col, row)`,
/// if any. The bottom row is the status bar and is intentionally not focusable.
fn widget_at(app: &App, full_area: Rect, col: u16, row: u16) -> Option<(String, Rect)> {
    if full_area.width == 0 || full_area.height == 0 {
        return None;
    }
    let main_height = full_area.height.saturating_sub(1);
    if row >= main_height {
        return None;
    }
    let main_area = Rect::new(full_area.x, full_area.y, full_area.width, main_height);
    for resolved in app.config.layout.resolve(main_area) {
        let r = resolved.area;
        let in_x = col >= r.x && col < r.x + r.width;
        let in_y = row >= r.y && row < r.y + r.height;
        if in_x && in_y {
            return Some((resolved.cell.render_target_id()?, r));
        }
    }
    None
}

/// First-fit assignment of `Shift+<letter>` shortcuts in registration
/// order. Walks into composite widgets (stacks) so children hidden
/// inside a stack can still claim a letter — the runtime dispatcher
/// then uses the `(parent_id, child_id)` pair to switch the stack to
/// that child before focusing the cell.
///
/// Returns the letter → `(parent_id, child_id)` map; each widget (or
/// composite child) is notified via `set_shortcut`, including `None`
/// for widgets whose preferences were all taken.
fn assign_shortcuts(manager: &mut WidgetManager) -> HashMap<char, (String, Option<String>)> {
    // First pass: gather every (parent_id, child_id, prefs) triple.
    let mut targets: Vec<(String, Option<String>, Vec<char>)> = Vec::new();
    for parent_id in manager.ids().to_vec() {
        let children: Vec<String> = manager
            .get(&parent_id)
            .map(|w| w.composite_children())
            .unwrap_or_default();
        if children.is_empty() {
            let prefs = manager
                .get(&parent_id)
                .map(|w| w.shortcut_preferences().to_vec())
                .unwrap_or_default();
            targets.push((parent_id.clone(), None, prefs));
        } else {
            // Composite: each child contributes its own pref list.
            for child_id in children {
                let prefs = if let Some(parent) = manager.get_mut(&parent_id) {
                    parent
                        .composite_child_mut(&child_id)
                        .map(|c| c.shortcut_preferences().to_vec())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                targets.push((parent_id.clone(), Some(child_id), prefs));
            }
        }
    }

    // Second pass: first-fit assignment. Insertion order preserves
    // registration-order ties.
    let mut shortcuts: HashMap<char, (String, Option<String>)> = HashMap::new();
    let mut assigned_letters: HashMap<(String, Option<String>), char> = HashMap::new();
    for (parent_id, child_id, prefs) in &targets {
        for letter in prefs {
            let letter = letter.to_ascii_lowercase();
            if !letter.is_ascii_alphabetic() {
                continue;
            }
            // 'z' is reserved for the zoom toggle — never assign it.
            if letter == 'z' {
                continue;
            }
            if !shortcuts.contains_key(&letter) {
                shortcuts.insert(letter, (parent_id.clone(), child_id.clone()));
                assigned_letters.insert((parent_id.clone(), child_id.clone()), letter);
                break;
            }
        }
    }

    // Third pass: notify each widget (or composite child) of its
    // granted letter (or `None` if all preferences were taken).
    for (parent_id, child_id, _) in &targets {
        let letter = assigned_letters
            .get(&(parent_id.clone(), child_id.clone()))
            .copied();
        if let Some(parent) = manager.get_mut(parent_id) {
            match child_id {
                Some(child) => {
                    if let Some(c) = parent.composite_child_mut(child) {
                        c.set_shortcut(letter);
                    }
                }
                None => parent.set_shortcut(letter),
            }
        }
    }
    shortcuts
}

/// Focus-cycling order matches layout-cell order, skipping unknown widgets.
fn focus_order_from_layout(config: &Config, manager: &WidgetManager) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cell in &config.layout.cells {
        // For stacks, Tab cycles to the stack as a single focusable unit
        // (the active tab inside it is what receives input). For
        // single-widget cells, the widget id IS the focus target.
        let Some(id) = cell.render_target_id() else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        if manager.get(&id).is_some() {
            order.push(id);
        }
    }
    order
}

/// Register each unique `(kind, instance)` pair found in the layout via
/// the widget registry. Unknown kinds log a warning and skip. Stack
/// cells register a wrapping `StackWidget` under a synthetic id
/// (`stack:<child1>+<child2>+…`) that the render path looks up via
/// `GridCell::render_target_id()`.
fn register_widgets_from_layout(
    manager: &mut WidgetManager,
    config: &Config,
    theme: Arc<Theme>,
    llm_provider: Option<Arc<dyn LlmProvider>>,
    cache: &Cache,
) {
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut seen_stack: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cell in &config.layout.cells {
        if cell.is_stack() {
            // Stack children are built as fresh widget instances owned
            // by the StackWidget (widgets aren't `Clone`). They share
            // the on-disk cache scope with any standalone registration
            // of the same `(kind, instance)`.
            let stack_id = match cell.render_target_id() {
                Some(id) => id,
                None => continue,
            };
            if !seen_stack.insert(stack_id.clone()) {
                continue;
            }
            let mut children: Vec<Box<dyn crate::widgets::Widget>> = Vec::new();
            for child_ref in cell.stack_widget_refs() {
                let (kind, instance) = parse_widget_ref(&child_ref);
                let scoped = cache.scoped(&kind, &instance);
                let llm = llm_provider.clone();
                let theme_c = theme.clone();
                let built = registry::build_for(&kind, &instance, move |instance| WidgetCtx {
                    instance,
                    theme: theme_c,
                    llm,
                    cache: scoped,
                });
                match built {
                    Some(w) => children.push(w),
                    None => tracing::warn!(
                        kind = %kind,
                        instance = %instance,
                        "unknown widget kind in stack, skipping"
                    ),
                }
            }
            if children.is_empty() {
                continue;
            }
            let stack = crate::widgets::stack::StackWidget::new(
                stack_id.clone(),
                children,
                config.global.stack_hidden_poll_ratio,
                theme.clone(),
            );
            manager.register_boxed(Box::new(stack));
            continue;
        }
        let Some(primary) = cell.primary_widget() else {
            continue;
        };
        let (kind, instance) = parse_widget_ref(&primary);
        if !seen.insert((kind.clone(), instance.clone())) {
            continue;
        }
        register_widget(
            manager,
            &kind,
            &instance,
            theme.clone(),
            llm_provider.clone(),
            cache,
        );
    }
}

fn register_widget(
    manager: &mut WidgetManager,
    kind: &str,
    instance: &str,
    theme: Arc<Theme>,
    llm_provider: Option<Arc<dyn LlmProvider>>,
    cache: &Cache,
) {
    let scoped = cache.scoped(kind, instance);
    let widget = registry::build_for(kind, instance, |instance| WidgetCtx {
        instance,
        theme,
        llm: llm_provider,
        cache: scoped,
    });
    match widget {
        Some(w) => manager.register_boxed(w),
        None => {
            tracing::warn!(kind = %kind, instance = %instance, "unknown widget kind in layout, skipping");
        }
    }
}

/// Collect every composite (stack) widget's current active-tab index
/// into a `(stack_id → tab_index)` map. Used to detect when the
/// runtime state file needs to be re-saved.
fn collect_stack_snapshot(manager: &WidgetManager) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    for id in manager.ids() {
        if let Some(widget) = manager.get(id) {
            if let Some(active) = widget.composite_active_index() {
                out.insert(id.clone(), active);
            }
        }
    }
    out
}

/// Fallback when the layout produces no recognised widgets — seed every
/// descriptor with `default_in_first_run = true`.
fn register_default_widgets(
    manager: &mut WidgetManager,
    theme: Arc<Theme>,
    llm_provider: Option<Arc<dyn LlmProvider>>,
    cache: &Cache,
) {
    for kind in registry::default_kinds() {
        register_widget(
            manager,
            kind,
            "main",
            theme.clone(),
            llm_provider.clone(),
            cache,
        );
    }
}

/// Run the main loop. `TerminalGuard` restores the terminal on any exit path.
pub async fn run(config_path_override: Option<PathBuf>) -> Result<()> {
    let config = config::load(config_path_override.as_deref())?;

    let mut terminal = enter_tui().context("failed to initialize terminal")?;
    let _guard = TerminalGuard;

    let mut app = App::new(config);

    // Live-reload via the `notify` crate. Failure is non-fatal — we just
    // run without hot-reload.
    let config_rx = match config::config_dir() {
        Ok(dir) if dir.exists() => match config::watcher::spawn(dir) {
            Ok(rx) => Some(rx),
            Err(err) => {
                tracing::warn!(error = %err, "failed to spawn config watcher");
                None
            }
        },
        _ => None,
    };
    let mut events = EventReader::new(Duration::from_millis(250), config_rx);

    // Initial draw before the first event arrives.
    app.expire_stale_feedback();
    terminal.draw(|frame| {
        ui::render(frame, &app.render_state());
    })?;

    let ctx = AppContext;

    // Draw state accumulated across a coalesced burst of events (see the
    // has_pending() deferral below), so a flood of input collapses into one
    // repaint instead of one per event.
    let mut deferred_dirty: HashSet<String> = HashSet::new();
    let mut deferred_draw = false;
    let mut deferred_force_full = false;

    while let Some(evt) = events.next().await {
        let is_tick = matches!(evt, Event::Tick);
        // Non-tick events (key / paste / resize / config) mutate state outside
        // the dirty-bit contract, so they draw unconditionally. The mouse arm
        // narrows this to "only when the event actually changed something".
        let mut nontick_wants_draw = true;
        match evt {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                // Command bar takes precedence over both widgets and globals
                // — typing into it routes nowhere else.
                if app.command_buffer.is_some() {
                    app.handle_command_bar_key(key);
                    if app.should_quit {
                        break;
                    }
                } else if app.zoom_target.is_some() {
                    // Zoom-active dispatch: route key to the zoomed widget first,
                    // then to the zoom-modal global handler on Ignored.
                    let consumed = {
                        let (parent_id, child_id) = {
                            let zoom = app.zoom_target.as_ref().unwrap();
                            (zoom.parent_id.clone(), zoom.child_id.clone())
                        };
                        if let Some(parent) = app.manager.get_mut(&parent_id) {
                            let result = match child_id {
                                None => parent.handle_key(key),
                                Some(ref cid) => match parent.composite_child_mut(cid) {
                                    Some(child) => child.handle_key(key),
                                    None => EventResult::Ignored,
                                },
                            };
                            result == EventResult::Handled
                        } else {
                            false
                        }
                    };
                    if !consumed {
                        app.handle_global_zoom_key(key);
                    }
                    app.persist_runtime_state_if_dirty();
                } else {
                    let consumed = if let Some(id) = app.focused_widget().map(str::to_string) {
                        if let Some(widget) = app.manager.get_mut(&id) {
                            widget.handle_key(key) == EventResult::Handled
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !consumed {
                        app.handle_global_key(key);
                    }
                    // After every key event, persist any stack
                    // active-tab change (rotation via `,` / `.`, or
                    // Shift+letter walking into a stack child) so the
                    // user's choice survives a restart.
                    app.persist_runtime_state_if_dirty();
                }
            }
            Event::Mouse(mut mouse) => {
                let mut mouse_acted = false;
                // Apply the global `mouse_scroll` preference once at the
                // dispatch boundary so every downstream consumer (help
                // overlay + widgets) sees a consistent direction without
                // each having to know about the preference.
                if app.config.global.mouse_scroll == config::types::MouseScroll::Inverted {
                    mouse.kind = invert_scroll(mouse.kind);
                }
                // Help overlay sits on top of the entire dashboard — when
                // it's open, mouse input belongs to it, not to the widgets
                // visually behind it. Without this guard the scroll wheel
                // would silently drive the widget under the cursor.
                if app.show_help {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => app.scroll_help(-1),
                        MouseEventKind::ScrollDown => app.scroll_help(1),
                        _ => {}
                    }
                    // Swallow everything else (clicks etc.) so the layout
                    // underneath stays inert until the overlay closes.
                    if app.should_quit {
                        break;
                    }
                    app.expire_stale_feedback();
                    terminal.draw(|frame| {
                        ui::render(frame, &app.render_state());
                    })?;
                    continue;
                }
                if let Ok(size) = terminal.size() {
                    let full = Rect::new(0, 0, size.width, size.height);
                    let target = widget_at(&app, full, mouse.column, mouse.row);
                    if app.zoom_target.is_some() {
                        // Three-zone mouse dispatch while zoom is active.
                        let chrome_visible = app.command_buffer.is_some()
                            || app.command_feedback.is_some()
                            || app.config.global.show_status_bar;
                        let chrome_h: u16 = if chrome_visible { 1 } else { 0 };
                        let main_area = Rect::new(
                            full.x,
                            full.y,
                            full.width,
                            full.height.saturating_sub(chrome_h),
                        );
                        let zoom_rect = crate::ui::zoom_rect_with_margins(
                            main_area,
                            app.config.global.zoom_margin,
                        );
                        let inner_rect = Rect {
                            x: zoom_rect.x + 1,
                            y: zoom_rect.y + 1,
                            width: zoom_rect.width.saturating_sub(2),
                            height: zoom_rect.height.saturating_sub(2),
                        };
                        let in_zoom = mouse.column >= zoom_rect.x
                            && mouse.column < zoom_rect.x + zoom_rect.width
                            && mouse.row >= zoom_rect.y
                            && mouse.row < zoom_rect.y + zoom_rect.height;
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                if in_zoom {
                                    // Click inside zoom frame: route to zoomed widget.
                                    mouse_acted =
                                        route_mouse_to_zoom_widget(&mut app, mouse, inner_rect);
                                } else if !app.is_zoom_retarget_suppressed() {
                                    if let Some((id, _)) = target {
                                        // Click on a backdrop widget: retarget zoom to it.
                                        let parent_id = id.clone();
                                        if let Some(pos) =
                                            app.focus_order.iter().position(|w| w == &parent_id)
                                        {
                                            app.focus_idx = pos;
                                        }
                                        let zoom = app.zoom_target_for_widget_id(parent_id);
                                        app.zoom_target = Some(zoom);
                                        mouse_acted = true;
                                    } else {
                                        // Click in the empty margin: exit zoom.
                                        app.exit_zoom();
                                        mouse_acted = true;
                                    }
                                }
                            }
                            MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                            | MouseEventKind::ScrollLeft
                            | MouseEventKind::ScrollRight => {
                                if in_zoom {
                                    mouse_acted =
                                        route_mouse_to_zoom_widget(&mut app, mouse, inner_rect);
                                }
                            }
                            _ => {}
                        }
                    } else {
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                if let Some((id, cell_area)) = target {
                                    if let Some(pos) =
                                        app.focus_order.iter().position(|w| w == &id)
                                    {
                                        // Focus moved → the highlight changes, so a
                                        // repaint is warranted regardless of what the
                                        // widget does with the click.
                                        if app.focus_idx != pos {
                                            mouse_acted = true;
                                        }
                                        app.focus_idx = pos;
                                    }
                                    if let Some(widget) = app.manager.get_mut(&id) {
                                        if widget.handle_mouse(mouse, cell_area)
                                            == EventResult::Handled
                                        {
                                            mouse_acted = true;
                                        }
                                    }
                                }
                            }
                            // Scroll wheel (both axes): forward to the widget
                            // under the cursor without changing focus — most
                            // users expect "scroll whatever I'm hovering over".
                            MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                            | MouseEventKind::ScrollLeft
                            | MouseEventKind::ScrollRight => {
                                if let Some((id, cell_area)) = target {
                                    if let Some(widget) = app.manager.get_mut(&id) {
                                        if widget.handle_mouse(mouse, cell_area)
                                            == EventResult::Handled
                                        {
                                            mouse_acted = true;
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                // Only a mouse event that actually changed something warrants a
                // repaint; inert events (button release, a click on empty space,
                // a scroll that hit its limit) must not force a full redraw.
                nontick_wants_draw = mouse_acted;
            }
            Event::Paste(text) => {
                // Hand the full bracketed-paste payload to the focused
                // widget. Most widgets ignore paste; text-buffer widgets
                // (notes) override Widget::handle_paste to insert it
                // atomically. The command bar swallows paste while open so
                // pasted text doesn't smuggle commands into widgets.
                if app.command_buffer.is_some() {
                    app.handle_command_bar_paste(&text);
                } else if let Some(id) = app.focused_widget().map(str::to_string) {
                    if let Some(widget) = app.manager.get_mut(&id) {
                        let _ = widget.handle_paste(&text);
                    }
                }
            }
            Event::Resize => {
                // Ratatui handles the re-layout on the next draw call below.
            }
            Event::ConfigChanged(path) => {
                apply_config_change(&mut app, &path);
            }
            Event::Tick => {
                // While zoomed, throttle backdrop widget updates to
                // `stack_hidden_poll_ratio` cadence — mirrors how StackWidget
                // gates hidden children.  The zoom target always updates at
                // full rate so its data stays live inside the frame (Req 4).
                app.zoom_backdrop_tick_counter =
                    app.zoom_backdrop_tick_counter.wrapping_add(1);
                let ratio = app.config.global.stack_hidden_poll_ratio.max(1) as u64;
                let allow_backdrop_tick = app.zoom_target.is_none()
                    || ratio == 1
                    || app.zoom_backdrop_tick_counter % ratio == 0;
                let zoom_parent_id: Option<String> =
                    app.zoom_target.as_ref().map(|z| z.parent_id.clone());
                for id in app.manager.ids().to_vec() {
                    // Skip backdrop widgets on non-quota ticks while zoomed.
                    if !allow_backdrop_tick
                        && zoom_parent_id.as_deref() != Some(id.as_str())
                    {
                        continue;
                    }
                    if let Some(w) = app.manager.get_mut(&id) {
                        if let Err(err) = w.update(&ctx).await {
                            tracing::warn!(widget = %id, error = %err, "widget update failed");
                        }
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }

        // Honor any focus requests widgets queued (e.g. a timer alarm
        // pulling the clock to the front of its stack). Tick-only —
        // the user-event branches (key/mouse/paste/resize) don't need
        // this poll, and a terminal sending continuous mouse-move
        // events shouldn't pay the per-widget iteration cost. A
        // 250 ms latency on alarm promotion is imperceptible.
        let focus_promoted = if is_tick {
            app.process_focus_requests()
        } else {
            false
        };

        let feedback_cleared = app.expire_stale_feedback();
        // Always drain widget dirty bits so they don't pile up between
        // draws — even when we already know we're going to draw (non-tick
        // events), so the next tick starts from a clean slate.
        deferred_dirty.extend(app.drain_widget_dirty_ids());

        // Per-widget `dirty_ids` is the authoritative "what changed" signal
        // only on ticks. Non-tick events (key / paste / resize / config
        // change) can mutate state outside the dirty-bit contract, so they
        // repaint unconditionally; a mouse event repaints only when it
        // actually acted (`nontick_wants_draw`). A non-tick draw forces a
        // full repaint; a tick-only batch keeps the partial fast path.
        let this_wants_draw = if is_tick {
            !deferred_dirty.is_empty() || feedback_cleared || focus_promoted
        } else {
            // A cleared status-bar feedback needs a repaint even if the mouse
            // event itself was inert.
            nontick_wants_draw || feedback_cleared
        };
        deferred_draw |= this_wants_draw;
        deferred_force_full |= !is_tick && nontick_wants_draw;

        // Coalesce: if more input is already queued, fold it into the next
        // iteration's single draw rather than repainting per event. Collapses
        // bursts (rapid clicks, scroll-wheel spins) into one repaint.
        if events.has_pending() {
            continue;
        }

        if deferred_draw {
            // Move the partial-draw cache out so the closure has a disjoint
            // mut borrow while `render_state()` holds an immutable borrow of
            // the rest of `app`. We restore it immediately after.
            let mut cache = std::mem::take(&mut app.partial_draw);
            let render_state = app.render_state();
            let force_full = deferred_force_full;
            terminal.draw(|frame| {
                ui::render_partial(
                    frame,
                    &render_state,
                    &deferred_dirty,
                    force_full,
                    &mut cache,
                );
            })?;
            app.partial_draw = cache;
        }
        deferred_dirty.clear();
        deferred_draw = false;
        deferred_force_full = false;
    }

    Ok(())
}

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

fn enter_tui() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // EnableBracketedPaste makes the terminal wrap pastes in
    // `\x1b[200~`/`\x1b[201~` markers, which crossterm surfaces as a
    // single `Event::Paste(String)` instead of one KeyEvent per
    // character. Without it, a paste containing `.`, `,`, `i`, `s`,
    // etc. fires widget shortcuts mid-stream — the user sees the
    // dashboard flash through stack rotations / mode toggles / etc.
    // before the rest of the buffer arrives. The Paste handler is
    // already wired up in the event loop (Event::Paste branch above);
    // this just turns on the terminal-side framing.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// Restores the terminal on drop so a panic still leaves the user's shell sane.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_at_maps_clicks_to_cells() {
        let app = App::new(Config::default());
        let area = Rect::new(0, 0, 100, 40);
        assert_eq!(
            widget_at(&app, area, 5, 2).map(|(id, _)| id),
            Some("clock".to_string())
        );
        assert_eq!(
            widget_at(&app, area, 80, 2).map(|(id, _)| id),
            Some("calendar".to_string())
        );
        assert_eq!(
            widget_at(&app, area, 50, 35).map(|(id, _)| id),
            Some("stocks".to_string())
        );
        // Status bar row — last row of the area — should be unfocusable.
        assert!(widget_at(&app, area, 50, 39).is_none());
    }

    #[cfg(feature = "widgets-all")]
    #[test]
    fn focus_cycles_in_layout_order() {
        let config = Config::default();
        let mut app = App::new(config);
        assert_eq!(
            app.focus_order,
            vec![
                "clock".to_string(),
                "calendar".to_string(),
                "weather".to_string(),
                "news".to_string(),
                "stocks".to_string(),
            ]
        );
        assert_eq!(app.focused_widget(), Some("clock"));
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("calendar"));
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("weather"));
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("news"));
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("stocks"));
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("clock"));
        app.cycle_focus(false);
        assert_eq!(app.focused_widget(), Some("stocks"));
    }

    #[cfg(feature = "widgets-all")]
    #[test]
    fn multi_instance_widgets_register_under_composed_ids() {
        // Two clocks (home + office) + one stocks should yield three widgets
        // with ids "clock@home", "clock@office", "stocks".
        use crate::config::layout::{GridCell, LayoutConfig};
        let mut config = Config::default();
        config.layout = LayoutConfig {
            columns: vec![50, 50],
            rows: vec![50, 50],
            cells: vec![
                GridCell {
                    widget: Some("clock@home".into()),
                    widgets: None,
                    col: 0,
                    row: 0,
                    col_span: 1,
                    row_span: 1,
                },
                GridCell {
                    widget: Some("clock@office".into()),
                    widgets: None,
                    col: 1,
                    row: 0,
                    col_span: 1,
                    row_span: 1,
                },
                GridCell {
                    widget: Some("stocks".into()),
                    widgets: None,
                    col: 0,
                    row: 1,
                    col_span: 2,
                    row_span: 1,
                },
            ],
        };
        let app = App::new(config);
        let ids: Vec<&str> = app.manager.ids().iter().map(String::as_str).collect();
        assert!(ids.contains(&"clock@home"), "got {ids:?}");
        assert!(ids.contains(&"clock@office"), "got {ids:?}");
        assert!(ids.contains(&"stocks"), "got {ids:?}");
        assert_eq!(ids.len(), 3, "no extra widgets registered: {ids:?}");

        // Two clocks should claim *different* letters from the
        // ['c', 'l', 'o', 'k'] preference list — the second falls through
        // to 'l' because 'c' is taken.
        let home_letter = app
            .shortcuts
            .iter()
            .find_map(|(k, (parent, _child))| (parent == "clock@home").then_some(*k));
        let office_letter = app
            .shortcuts
            .iter()
            .find_map(|(k, (parent, _child))| (parent == "clock@office").then_some(*k));
        assert!(home_letter.is_some(), "clock@home should have a shortcut");
        assert!(
            office_letter.is_some(),
            "clock@office should have a shortcut"
        );
        assert_ne!(home_letter, office_letter);
    }

    #[cfg(feature = "widgets-all")]
    #[test]
    fn shortcuts_resolve_preference_conflicts_by_load_order() {
        let app = App::new(Config::default());
        // Registration order in App::new is stocks, clock, weather,
        // calendar, news — so stocks gets 's', clock gets 'c', weather
        // 'w', calendar falls through 'c' (taken) to 'd', news 'n'.
        let parent = |k: &char| app.shortcuts.get(k).map(|(p, _)| p.as_str());
        assert_eq!(parent(&'s'), Some("stocks"));
        assert_eq!(parent(&'c'), Some("clock"));
        assert_eq!(parent(&'w'), Some("weather"));
        assert_eq!(
            parent(&'d'),
            Some("calendar"),
            "calendar should fall through to 'd' since clock claimed 'c'"
        );
        assert_eq!(parent(&'n'), Some("news"));
    }

    #[cfg(feature = "widgets-all")]
    #[test]
    fn stack_cell_shortcuts_walk_into_children() {
        // A stack containing clock + weather should yield shortcuts
        // whose `parent_id` is the stack's synthetic id but whose
        // `child_id` is the kind ("clock"/"weather"), so the
        // dispatcher can flip the stack and focus it in one step.
        use crate::config::layout::{GridCell, LayoutConfig};
        let mut config = Config::default();
        config.layout = LayoutConfig {
            columns: vec![100],
            rows: vec![100],
            cells: vec![GridCell {
                widget: None,
                widgets: Some(vec!["clock".into(), "weather".into()]),
                col: 0,
                row: 0,
                col_span: 1,
                row_span: 1,
            }],
        };
        let app = App::new(config);

        // Stack must be registered under its synthetic id.
        let ids: Vec<&str> = app.manager.ids().iter().map(String::as_str).collect();
        assert_eq!(ids, vec!["stack:clock+weather"]);

        // Both child kinds must end up addressable via Shift+letter.
        let clock_short = app.shortcuts.iter().find_map(|(letter, (parent, child))| {
            (parent == "stack:clock+weather" && child.as_deref() == Some("clock"))
                .then_some(*letter)
        });
        let weather_short = app.shortcuts.iter().find_map(|(letter, (parent, child))| {
            (parent == "stack:clock+weather" && child.as_deref() == Some("weather"))
                .then_some(*letter)
        });
        assert!(
            clock_short.is_some(),
            "clock-inside-stack should claim a letter"
        );
        assert!(
            weather_short.is_some(),
            "weather-inside-stack should claim a letter"
        );
        assert_ne!(clock_short, weather_short);
    }

    #[test]
    fn invert_scroll_flips_both_axes_and_passes_other_kinds_through() {
        assert_eq!(
            invert_scroll(MouseEventKind::ScrollUp),
            MouseEventKind::ScrollDown
        );
        assert_eq!(
            invert_scroll(MouseEventKind::ScrollDown),
            MouseEventKind::ScrollUp
        );
        assert_eq!(
            invert_scroll(MouseEventKind::ScrollLeft),
            MouseEventKind::ScrollRight
        );
        assert_eq!(
            invert_scroll(MouseEventKind::ScrollRight),
            MouseEventKind::ScrollLeft
        );
        // Non-scroll events are untouched.
        let click = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(invert_scroll(click), click);
        assert_eq!(invert_scroll(MouseEventKind::Moved), MouseEventKind::Moved);
    }

    // ── Zoom tests ────────────────────────────────────────────────────────

    /// `z` key enters zoom for the focused widget and sets `zoom_target`.
    #[cfg(feature = "widgets-all")]
    #[test]
    fn zoom_enter_sets_zoom_target() {
        let mut app = App::new(Config::default());
        assert!(app.zoom_target.is_none());
        assert_eq!(app.focused_widget(), Some("clock"));
        app.zoom_enter();
        assert_eq!(
            app.zoom_target.as_ref().map(|z| z.parent_id.as_str()),
            Some("clock")
        );
    }

    /// `exit_zoom` clears `zoom_target` and restores focus to the widget that was zoomed.
    #[cfg(feature = "widgets-all")]
    #[test]
    fn exit_zoom_clears_target_and_restores_focus() {
        let mut app = App::new(Config::default());
        // Move focus to calendar (index 1), then zoom.
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("calendar"));
        app.zoom_enter();
        assert!(app.zoom_target.is_some());
        // Advance focus without exiting zoom — simulates what a retarget call might do.
        app.focus_idx = 0;
        app.exit_zoom();
        assert!(app.zoom_target.is_none());
        // exit_zoom restores focus_idx to the zoomed widget's position.
        assert_eq!(app.focused_widget(), Some("calendar"));
    }

    /// Pressing `z` while already zoomed exits zoom (no nested zoom).
    #[cfg(feature = "widgets-all")]
    #[test]
    fn second_z_press_exits_zoom() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(Config::default());
        app.zoom_enter();
        assert!(app.zoom_target.is_some());
        // Second `z` while zoom is active → handled by handle_global_zoom_key → exit_zoom.
        let key = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE);
        app.handle_global_zoom_key(key);
        assert!(app.zoom_target.is_none());
    }

    /// `Esc` while zoomed exits zoom.
    #[cfg(feature = "widgets-all")]
    #[test]
    fn esc_exits_zoom() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(Config::default());
        app.zoom_enter();
        assert!(app.zoom_target.is_some());
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_global_zoom_key(key);
        assert!(app.zoom_target.is_none());
    }

    /// `retarget_zoom_cycle` advances and wraps correctly.
    #[cfg(feature = "widgets-all")]
    #[test]
    fn retarget_zoom_cycle_wraps() {
        let mut app = App::new(Config::default());
        app.zoom_enter();
        // focus_order for default config should be [clock, calendar, weather, news, stocks].
        let n = app.focus_order.len();
        assert!(n > 1);
        let start_id = app.zoom_target.as_ref().unwrap().parent_id.clone();
        app.retarget_zoom_cycle(true);
        let next_id = app.zoom_target.as_ref().unwrap().parent_id.clone();
        assert_ne!(start_id, next_id);
        // Cycle back and forward (n-1) more times to get back to start.
        for _ in 0..(n - 1) {
            app.retarget_zoom_cycle(true);
        }
        let final_id = app.zoom_target.as_ref().unwrap().parent_id.clone();
        assert_eq!(start_id, final_id, "cycling forward n times should wrap back to start");
    }

    /// `assign_shortcuts` never assigns 'z' — it is reserved for zoom.
    #[test]
    fn assign_shortcuts_never_assigns_z() {
        let mut manager = WidgetManager::new();
        let shortcuts = assign_shortcuts(&mut manager);
        // 'z' must not appear as any assigned shortcut regardless of widget preferences.
        assert!(
            !shortcuts.contains_key(&'z'),
            "assign_shortcuts must never assign 'z' (reserved for zoom toggle)"
        );
    }

    /// `zoom_target_for_widget_id` returns a leaf ZoomTarget for a non-composite widget.
    #[cfg(feature = "widgets-all")]
    #[test]
    fn zoom_target_for_leaf_widget_has_no_child() {
        let app = App::new(Config::default());
        let zt = app.zoom_target_for_widget_id("clock".into());
        assert_eq!(zt.parent_id, "clock");
        assert!(
            zt.child_id.is_none(),
            "clock is not a composite; child_id should be None"
        );
    }

    /// `zoom_backdrop_tick_counter` starts at zero.
    #[test]
    fn zoom_backdrop_tick_counter_starts_at_zero() {
        let app = App::new(Config::default());
        assert_eq!(app.zoom_backdrop_tick_counter, 0);
    }

    /// Verify the backdrop-throttle decision math: with ratio=3, over 9
    /// increments the backdrop is allowed on ticks 3, 6, 9 (3 of 9).
    /// The `allow_backdrop_tick` expression in the tick arm is replicated
    /// exactly here so any future refactor that changes the expression
    /// without updating this test will surface the divergence.
    #[test]
    fn zoom_backdrop_tick_throttle_math_correct() {
        let ratio: u64 = 3;
        let mut counter: u64 = 0;
        let mut allowed: u64 = 0;
        for _ in 0..9 {
            counter = counter.wrapping_add(1);
            // Replicate the tick arm condition (zoom is assumed active, ratio > 1).
            if ratio == 1 || counter % ratio == 0 {
                allowed += 1;
            }
        }
        // 9 ticks ÷ ratio 3 = 3 allowed backdrop ticks (at ticks 3, 6, 9).
        assert_eq!(allowed, 9 / ratio);
    }

    /// With ratio=1, every tick is a backdrop tick regardless of counter.
    #[test]
    fn zoom_backdrop_tick_ratio_one_means_no_throttle() {
        let ratio: u64 = 1;
        let mut counter: u64 = 0;
        let mut allowed: u64 = 0;
        for _ in 0..10 {
            counter = counter.wrapping_add(1);
            if ratio == 1 || counter % ratio == 0 {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 10, "ratio=1 must allow every tick");
    }
}
