//! Unified event stream — terminal events + a periodic tick.
//!
//! The render loop wants ONE pollable source. Crossterm gives us
//! terminal events via `EventStream`; we layer a tokio `interval` on
//! top so the screen can refresh time-relative state (uptime,
//! "drained 4s ago") without waiting for a keystroke.

use std::time::Duration;

use crossterm::event::{Event, EventStream as CtEventStream};
use futures::StreamExt;
use tokio::time::{interval, Interval};

#[derive(Debug)]
pub enum TuiEvent {
    Tick,
    Term(Event),
}

pub struct EventStream {
    ct: CtEventStream,
    ticker: Interval,
}

impl EventStream {
    pub fn new(tick: Duration) -> Self {
        Self {
            ct: CtEventStream::new(),
            ticker: interval(tick),
        }
    }

    pub async fn next(&mut self) -> Option<TuiEvent> {
        tokio::select! {
            biased;
            ev = self.ct.next() => {
                match ev {
                    Some(Ok(e)) => Some(TuiEvent::Term(e)),
                    Some(Err(_)) => None,
                    None => None,
                }
            }
            _ = self.ticker.tick() => Some(TuiEvent::Tick),
        }
    }
}
