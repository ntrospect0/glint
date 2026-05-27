use anyhow::Result;
use async_trait::async_trait;
use crossterm::event::KeyEvent;
use ratatui::{
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::ui::{decorate_title, focus_border_style};

use super::{AppContext, EventResult, Widget};

/// Phase 1 placeholder. The real implementation will fetch quotes via
/// `YahooFinanceProvider` and render an intraday graph + watchlist.
pub struct StocksWidget {
    id: String,
}

impl Default for StocksWidget {
    fn default() -> Self {
        Self {
            id: "stocks".into(),
        }
    }
}

impl StocksWidget {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Widget for StocksWidget {
    fn id(&self) -> &str {
        &self.id
    }

    fn display_name(&self) -> &str {
        "Stocks"
    }

    async fn update(&mut self, _ctx: &AppContext) -> Result<()> {
        Ok(())
    }

    fn render(&self, frame: &mut Frame, area: Rect, focused: bool) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(focus_border_style(focused))
            .title(Span::styled(
                decorate_title(focused, "Stocks"),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        let body = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "Stocks widget",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("No quotes yet — Phase 2 wires up Yahoo Finance."),
            Line::from(""),
            Line::from("Tab to cycle widgets · q to quit · ? for help"),
        ])
        .alignment(Alignment::Center)
        .block(block);
        frame.render_widget(body, area);
    }

    fn handle_key(&mut self, _key: KeyEvent) -> EventResult {
        EventResult::Ignored
    }

    fn handle_command(&mut self, _cmd: &str, _args: &[&str]) -> Result<bool> {
        Ok(false)
    }

    fn config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    fn apply_config(&mut self, _config: serde_json::Value) -> Result<()> {
        Ok(())
    }
}
