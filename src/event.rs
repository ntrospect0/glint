// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ntrospect0

use std::{path::PathBuf, time::Duration};

use crossterm::event::{Event as CtEvent, EventStream, KeyEvent, MouseEvent};
use futures::StreamExt;
use tokio::sync::mpsc;

/// Events delivered to the main loop.
#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
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
        }
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
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
                let mapped = match evt {
                    CtEvent::Key(k) => Some(Event::Key(k)),
                    CtEvent::Mouse(m) => Some(Event::Mouse(m)),
                    CtEvent::Resize(_, _) => Some(Event::Resize),
                    _ => None,
                };
                if let Some(e) = mapped {
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
