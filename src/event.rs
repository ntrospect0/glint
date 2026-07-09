// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::{path::PathBuf, time::Duration};

use crossterm::event::{Event as CtEvent, EventStream, KeyEvent, MouseEvent, MouseEventKind};
use futures::StreamExt;
use tokio::sync::mpsc;

/// Events delivered to the main loop.
#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// Bracketed-paste payload from the terminal — the entire pasted text
    /// arrives atomically here instead of streaming as fake keystrokes,
    /// which is what made multi-line pastes into text widgets misbehave.
    Paste(String),
    /// Terminal resize. Ratatui recomputes layout from the next `terminal.size()`
    /// on draw, so the new dimensions don't need to ride the event.
    Resize,
    Tick,
    /// One of the user's TOML config files changed — main loop will re-read
    /// and hot-apply via Widget::apply_config.
    ConfigChanged(PathBuf),
}

/// Background reader that fans crossterm events + periodic ticks + config
/// file changes onto a single mpsc channel consumed by the main loop.
pub struct EventReader {
    rx: mpsc::Receiver<Event>,
    _handle: tokio::task::JoinHandle<()>,
    /// One-slot lookahead: `has_pending` peeks the next event by pulling it
    /// here, and `next` returns it before touching the channel.
    buffered: Option<Event>,
}

impl EventReader {
    pub fn new(tick_rate: Duration, config_changes: Option<mpsc::Receiver<PathBuf>>) -> Self {
        let (tx, rx) = mpsc::channel(64);
        let handle = tokio::spawn(async move {
            run_loop(tx, tick_rate, config_changes).await;
        });
        Self {
            rx,
            _handle: handle,
            buffered: None,
        }
    }

    pub async fn next(&mut self) -> Option<Event> {
        if let Some(evt) = self.buffered.take() {
            return Some(evt);
        }
        self.rx.recv().await
    }

    /// Non-blocking check for whether another event is already queued. Lets the
    /// main loop coalesce a burst of input into a single repaint. A peeked
    /// event is buffered so the next `next()` still returns it.
    pub fn has_pending(&mut self) -> bool {
        if self.buffered.is_some() {
            return true;
        }
        match self.rx.try_recv() {
            Ok(evt) => {
                self.buffered = Some(evt);
                true
            }
            Err(_) => false,
        }
    }
}

/// Map a crossterm event to our `Event`, or `None` to drop it. Mouse
/// motion/drag reports are dropped: nothing in the app consumes them, and
/// terminals can emit a continuous stream that would otherwise force a repaint
/// per report and starve real input.
fn map_ct_event(evt: CtEvent) -> Option<Event> {
    match evt {
        CtEvent::Key(k) => Some(Event::Key(k)),
        CtEvent::Mouse(m)
            if matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) =>
        {
            None
        }
        CtEvent::Mouse(m) => Some(Event::Mouse(m)),
        CtEvent::Paste(text) => Some(Event::Paste(text)),
        CtEvent::Resize(_, _) => Some(Event::Resize),
        _ => None,
    }
}

async fn run_loop(
    tx: mpsc::Sender<Event>,
    tick_rate: Duration,
    mut config_changes: Option<mpsc::Receiver<PathBuf>>,
) {
    let mut crossterm_events = EventStream::new();
    let mut ticker = tokio::time::interval(tick_rate);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_evt = crossterm_events.next() => {
                let Some(evt) = maybe_evt else { break };
                let Ok(evt) = evt else { continue };
                if let Some(e) = map_ct_event(evt) {
                    if tx.send(e).await.is_err() {
                        break;
                    }
                }
            }
            _ = ticker.tick() => {
                if tx.send(Event::Tick).await.is_err() {
                    break;
                }
            }
            maybe_path = async { config_changes.as_mut()?.recv().await }, if config_changes.is_some() => {
                let Some(path) = maybe_path else {
                    // Watcher dropped — keep going without it.
                    config_changes = None;
                    continue;
                };
                if tx.send(Event::ConfigChanged(path)).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent};

    fn mouse(kind: MouseEventKind) -> CtEvent {
        CtEvent::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
    }

    #[test]
    fn motion_and_drag_events_are_dropped() {
        assert!(map_ct_event(mouse(MouseEventKind::Moved)).is_none());
        assert!(map_ct_event(mouse(MouseEventKind::Drag(MouseButton::Left))).is_none());
    }

    #[test]
    fn clicks_scrolls_and_keys_pass_through() {
        assert!(matches!(
            map_ct_event(mouse(MouseEventKind::Down(MouseButton::Left))),
            Some(Event::Mouse(_))
        ));
        assert!(matches!(
            map_ct_event(mouse(MouseEventKind::Up(MouseButton::Left))),
            Some(Event::Mouse(_))
        ));
        assert!(matches!(
            map_ct_event(mouse(MouseEventKind::ScrollUp)),
            Some(Event::Mouse(_))
        ));
        assert!(matches!(
            map_ct_event(CtEvent::Key(KeyEvent::from(KeyCode::Char('a')))),
            Some(Event::Key(_))
        ));
        assert!(matches!(
            map_ct_event(CtEvent::Resize(80, 24)),
            Some(Event::Resize)
        ));
    }
}
