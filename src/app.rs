use std::{cell::Cell, collections::HashMap, io, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableMouseCapture, EnableMouseCapture, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
        MouseEventKind,
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
};

pub struct App {
    config: Config,
    theme: Arc<Theme>,
    manager: WidgetManager,
    focus_idx: usize,
    /// Widget ids in layout order (Tab cycles through this).
    focus_order: Vec<String>,
    /// `Shift+<letter>` → widget id. First registered widget wins on conflicts.
    shortcuts: HashMap<char, String>,
    should_quit: bool,
    show_help: bool,
    help_scroll: u16,
    /// Max scroll updated by `ui::help::render` so the scroll handler can
    /// clamp without re-computing the layout.
    help_scroll_max: Cell<u16>,
    /// `Some` while the user is composing after pressing `:`.
    command_buffer: Option<String>,
    /// Transient feedback shown next to the command bar; cleared on next key.
    command_feedback: Option<String>,
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

        let focus_order = focus_order_from_layout(&config, &manager);
        let shortcuts = assign_shortcuts(&mut manager);
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
        }
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
            // `Shift+<letter>` jumps to the widget that claimed that letter.
            // Some terminals drop the SHIFT modifier on shifted alphabetic
            // keys, so we match on case rather than `KeyModifiers::SHIFT`.
            (_, KeyCode::Char(c)) if c.is_ascii_uppercase() => {
                let lower = c.to_ascii_lowercase();
                if let Some(id) = self.shortcuts.get(&lower).cloned() {
                    if let Some(pos) = self.focus_order.iter().position(|w| w == &id) {
                        self.focus_idx = pos;
                    }
                }
            }
            _ => {}
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
                self.command_feedback = Some(format!("colorschemes.toml: {err}"));
                return;
            }
        };

        // Sort once — used by both the "no arg" hint and the "not found"
        // message so the order is stable from the user's perspective.
        let mut available: Vec<&str> = file.schemes.keys().map(String::as_str).collect();
        available.sort_unstable();
        let available_csv = available.join(", ");

        let Some(name) = args.first() else {
            self.command_feedback = if available.is_empty() {
                Some("usage: :scheme <name> — (no schemes defined in colorschemes.toml)".into())
            } else {
                Some(format!("usage: :scheme <name>. Available: {available_csv}"))
            };
            return;
        };

        let Some(scheme) = file.schemes.get(*name) else {
            self.command_feedback = if available.is_empty() {
                Some(format!(
                    "unknown scheme {name:?} — colorschemes.toml has no [schemes.*] blocks"
                ))
            } else {
                Some(format!(
                    "unknown scheme {name:?}. Available: {available_csv}"
                ))
            };
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
                self.command_feedback = Some(format!("scheme → {name}"));
            }
            Err(err) => {
                tracing::warn!(error = %err, scheme = %name, "failed to persist scheme");
                self.command_feedback = Some(format!(
                    "scheme → {name} (not persisted: {err})"
                ));
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
                    self.command_feedback = Some(format!("{id}: {err}"));
                    return;
                }
            }
        }
        self.command_feedback = Some(format!("unknown command: {cmd:?}"));
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
            return Some((resolved.cell.widget.clone(), r));
        }
    }
    None
}

/// First-fit assignment of `Shift+<letter>` shortcuts in registration order.
/// Returns the letter → id map; each widget is notified via `set_shortcut`
/// (including `None` for widgets whose preferences were all taken).
fn assign_shortcuts(manager: &mut WidgetManager) -> HashMap<char, String> {
    let mut shortcuts: HashMap<char, String> = HashMap::new();
    let mut assignments: HashMap<String, char> = HashMap::new();
    for id in manager.ids().to_vec() {
        let prefs: Vec<char> = manager
            .get(&id)
            .map(|w| w.shortcut_preferences().to_vec())
            .unwrap_or_default();
        for letter in prefs {
            let letter = letter.to_ascii_lowercase();
            if !letter.is_ascii_alphabetic() {
                continue;
            }
            if !shortcuts.contains_key(&letter) {
                shortcuts.insert(letter, id.clone());
                assignments.insert(id.clone(), letter);
                break;
            }
        }
    }
    for id in manager.ids().to_vec() {
        let letter = assignments.get(&id).copied();
        if let Some(widget) = manager.get_mut(&id) {
            widget.set_shortcut(letter);
        }
    }
    shortcuts
}

/// Focus-cycling order matches layout-cell order, skipping unknown widgets.
fn focus_order_from_layout(config: &Config, manager: &WidgetManager) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cell in &config.layout.cells {
        let (kind, instance) = parse_widget_ref(&cell.widget);
        let id = if instance == "main" {
            kind
        } else {
            format!("{kind}@{instance}")
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
/// the widget registry. Unknown kinds log a warning and skip.
fn register_widgets_from_layout(
    manager: &mut WidgetManager,
    config: &Config,
    theme: Arc<Theme>,
    llm_provider: Option<Arc<dyn LlmProvider>>,
    cache: &Cache,
) {
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for cell in &config.layout.cells {
        let (kind, instance) = parse_widget_ref(&cell.widget);
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
    terminal.draw(|frame| {
        ui::render(
            frame,
            &ui::RenderState {
                layout: &app.config.layout,
                manager: &app.manager,
                focused: app.focused_widget(),
                show_help: app.show_help,
                command_buffer: app.command_buffer.as_deref(),
                command_feedback: app.command_feedback.as_deref(),
                theme: &app.theme,
                theme_name: &app.config.global.theme,
                help_scroll: app.help_scroll,
                help_scroll_max: &app.help_scroll_max,
            },
        );
    })?;

    let ctx = AppContext;

    while let Some(evt) = events.next().await {
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
                }
            }
            Event::Mouse(mut mouse) => {
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
                    terminal.draw(|frame| {
                        ui::render(
                            frame,
                            &ui::RenderState {
                                layout: &app.config.layout,
                                manager: &app.manager,
                                focused: app.focused_widget(),
                                                show_help: app.show_help,
                                command_buffer: app.command_buffer.as_deref(),
                                command_feedback: app.command_feedback.as_deref(),
                                theme: &app.theme,
                                theme_name: &app.config.global.theme,
                                help_scroll: app.help_scroll,
                                help_scroll_max: &app.help_scroll_max,
                            },
                        );
                    })?;
                    continue;
                }
                if let Ok(size) = terminal.size() {
                    let full = Rect::new(0, 0, size.width, size.height);
                    let target = widget_at(&app, full, mouse.column, mouse.row);
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            if let Some((id, cell_area)) = target {
                                if let Some(pos) = app.focus_order.iter().position(|w| w == &id) {
                                    app.focus_idx = pos;
                                }
                                if let Some(widget) = app.manager.get_mut(&id) {
                                    let _ = widget.handle_mouse(mouse, cell_area);
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
                                    let _ = widget.handle_mouse(mouse, cell_area);
                                }
                            }
                        }
                        _ => {}
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
                for id in app.manager.ids().to_vec() {
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

        terminal.draw(|frame| {
            ui::render(
                frame,
                &ui::RenderState {
                    layout: &app.config.layout,
                    manager: &app.manager,
                    focused: app.focused_widget(),
                        show_help: app.show_help,
                    command_buffer: app.command_buffer.as_deref(),
                    command_feedback: app.command_feedback.as_deref(),
                    theme: &app.theme,
                    theme_name: &app.config.global.theme,
                    help_scroll: app.help_scroll,
                    help_scroll_max: &app.help_scroll_max,
                },
            );
        })?;
    }

    Ok(())
}

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

fn enter_tui() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// Restores the terminal on drop so a panic still leaves the user's shell sane.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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
                    widget: "clock@home".into(),
                    col: 0,
                    row: 0,
                    col_span: 1,
                    row_span: 1,
                },
                GridCell {
                    widget: "clock@office".into(),
                    col: 1,
                    row: 0,
                    col_span: 1,
                    row_span: 1,
                },
                GridCell {
                    widget: "stocks".into(),
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
            .find_map(|(k, v)| (v == "clock@home").then_some(*k));
        let office_letter = app
            .shortcuts
            .iter()
            .find_map(|(k, v)| (v == "clock@office").then_some(*k));
        assert!(home_letter.is_some(), "clock@home should have a shortcut");
        assert!(office_letter.is_some(), "clock@office should have a shortcut");
        assert_ne!(home_letter, office_letter);
    }

    #[test]
    fn shortcuts_resolve_preference_conflicts_by_load_order() {
        let app = App::new(Config::default());
        // Registration order in App::new is stocks, clock, weather,
        // calendar, news — so stocks gets 's', clock gets 'c', weather
        // 'w', calendar falls through 'c' (taken) to 'd', news 'n'.
        assert_eq!(app.shortcuts.get(&'s').map(String::as_str), Some("stocks"));
        assert_eq!(app.shortcuts.get(&'c').map(String::as_str), Some("clock"));
        assert_eq!(app.shortcuts.get(&'w').map(String::as_str), Some("weather"));
        assert_eq!(
            app.shortcuts.get(&'d').map(String::as_str),
            Some("calendar"),
            "calendar should fall through to 'd' since clock claimed 'c'"
        );
        assert_eq!(app.shortcuts.get(&'n').map(String::as_str), Some("news"));
    }

    #[test]
    fn invert_scroll_flips_both_axes_and_passes_other_kinds_through() {
        assert_eq!(invert_scroll(MouseEventKind::ScrollUp), MouseEventKind::ScrollDown);
        assert_eq!(invert_scroll(MouseEventKind::ScrollDown), MouseEventKind::ScrollUp);
        assert_eq!(invert_scroll(MouseEventKind::ScrollLeft), MouseEventKind::ScrollRight);
        assert_eq!(invert_scroll(MouseEventKind::ScrollRight), MouseEventKind::ScrollLeft);
        // Non-scroll events are untouched.
        let click = MouseEventKind::Down(MouseButton::Left);
        assert_eq!(invert_scroll(click), click);
        assert_eq!(invert_scroll(MouseEventKind::Moved), MouseEventKind::Moved);
    }
}
