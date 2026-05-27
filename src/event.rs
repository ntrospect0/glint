use std::time::Duration;

use crossterm::event::{Event as CtEvent, EventStream, KeyEvent, MouseEvent};
use futures::StreamExt;
use tokio::sync::mpsc;

/// Events delivered to the main loop.
#[derive(Debug, Clone)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(#[allow(dead_code)] u16, #[allow(dead_code)] u16),
    Tick,
}

/// Background reader that fans crossterm events + periodic ticks onto a single
/// mpsc channel consumed by the main loop.
pub struct EventReader {
    rx: mpsc::Receiver<Event>,
    _handle: tokio::task::JoinHandle<()>,
}

impl EventReader {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel(64);
        let handle = tokio::spawn(async move {
            run_loop(tx, tick_rate).await;
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

async fn run_loop(tx: mpsc::Sender<Event>, tick_rate: Duration) {
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
                    CtEvent::Resize(w, h) => Some(Event::Resize(w, h)),
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
        }
    }
}
