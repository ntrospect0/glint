use std::{io, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    event::{KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use chrono::Local;

use crate::{
    config::{self, Config},
    event::{Event, EventReader},
    ui,
    widgets::{
        calendar::{CalendarConfig, CalendarWidget},
        clock::{ClockConfig, ClockWidget},
        stocks::StocksWidget,
        weather::{WeatherConfig, WeatherWidget},
        AppContext, EventResult, WidgetManager,
    },
};

/// Top-level app state. Grows as the command bar, status bar, and help
/// overlay land in subsequent phases.
pub struct App {
    config: Config,
    manager: WidgetManager,
    focus_idx: usize,
    /// Widget ids in the order they appear in the grid (Tab cycling order).
    focus_order: Vec<String>,
    should_quit: bool,
    /// Set on every successful tick to drive the status-bar "Last fetch" field.
    last_fetch: Option<chrono::DateTime<Local>>,
}

impl App {
    pub fn new(config: Config) -> Self {
        let clock_cfg: ClockConfig = config::load_widget_toml("clock").unwrap_or_default();
        let weather_cfg: WeatherConfig = config::load_widget_toml("weather").unwrap_or_default();
        let calendar_cfg: CalendarConfig =
            config::load_widget_toml("calendar").unwrap_or_default();

        let mut manager = WidgetManager::new();
        manager.register(StocksWidget::new());
        manager.register(ClockWidget::with_config(clock_cfg));
        manager.register(WeatherWidget::with_config(weather_cfg));
        manager.register(CalendarWidget::with_config(calendar_cfg));

        let focus_order = focus_order_from_layout(&config, &manager);
        Self {
            config,
            manager,
            focus_idx: 0,
            focus_order,
            should_quit: false,
            last_fetch: None,
        }
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
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('q')) => self.should_quit = true,
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => self.should_quit = true,
            (KeyModifiers::NONE, KeyCode::Tab) => self.cycle_focus(true),
            (KeyModifiers::SHIFT, KeyCode::BackTab) | (KeyModifiers::NONE, KeyCode::BackTab) => {
                self.cycle_focus(false)
            }
            _ => {}
        }
    }
}

/// Build the focus-cycling order from the layout cells, keeping only widgets
/// that the manager actually knows about.
fn focus_order_from_layout(config: &Config, manager: &WidgetManager) -> Vec<String> {
    config
        .layout
        .cells
        .iter()
        .filter_map(|c| {
            manager
                .get(&c.widget)
                .map(|w| w.id().to_string())
        })
        .collect()
}

/// Set up the terminal, run the main loop, then tear the terminal back down
/// regardless of how we exited (panic-safe via the `TerminalGuard`).
pub async fn run(config_path_override: Option<PathBuf>) -> Result<()> {
    let config = config::load(config_path_override.as_deref())?;

    let mut terminal = enter_tui().context("failed to initialize terminal")?;
    let _guard = TerminalGuard;

    let mut app = App::new(config);
    let mut events = EventReader::new(Duration::from_millis(250));

    // Initial draw before the first event arrives.
    terminal.draw(|frame| {
        ui::render(
            frame,
            &app.config.layout,
            &app.manager,
            app.focused_widget(),
            app.last_fetch,
        );
    })?;

    let ctx = AppContext;

    while let Some(evt) = events.next().await {
        match evt {
            Event::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
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
            Event::Resize(_, _) => {
                // Ratatui handles the re-layout on the next draw call below.
            }
            Event::Tick => {
                for id in app.manager.ids().to_vec() {
                    if let Some(w) = app.manager.get_mut(&id) {
                        if let Err(err) = w.update(&ctx).await {
                            tracing::warn!(widget = %id, error = %err, "widget update failed");
                        }
                    }
                }
                app.last_fetch = Some(Local::now());
            }
        }

        if app.should_quit {
            break;
        }

        terminal.draw(|frame| {
            ui::render(
                frame,
                &app.config.layout,
                &app.manager,
                app.focused_widget(),
                app.last_fetch,
            );
        })?;
    }

    Ok(())
}

type Tui = Terminal<CrosstermBackend<io::Stdout>>;

fn enter_tui() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

/// Restores the terminal on drop so a panic still leaves the user's shell sane.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "stocks".to_string(),
            ]
        );
        assert_eq!(app.focused_widget(), Some("clock"));
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("calendar"));
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("weather"));
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("stocks"));
        app.cycle_focus(true);
        assert_eq!(app.focused_widget(), Some("clock"));
        app.cycle_focus(false);
        assert_eq!(app.focused_widget(), Some("stocks"));
    }
}
