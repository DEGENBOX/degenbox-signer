//! `tracing` layer that buffers the last N log lines in a ring so the
//! Logs tab can render them.
//!
//! We don't try to be clever — log lines are short, the ring is
//! capped at 500 entries, and the writer just formats `{level} {target}: {msg}`.

use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

pub const MAX_LINES: usize = 500;

#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: Level,
    pub target: String,
    pub message: String,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Vec<LogLine>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::with_capacity(MAX_LINES))),
        }
    }

    pub fn push(&self, line: LogLine) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() >= MAX_LINES {
            g.remove(0);
        }
        g.push(line);
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        g.clone()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

struct CaptureLayer {
    buf: LogBuffer,
}

struct MessageVisitor {
    out: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.out = format!("{value:?}");
            // strip the surrounding quotes Debug adds for `&str`.
            if self.out.starts_with('"') && self.out.ends_with('"') && self.out.len() >= 2 {
                self.out = self.out[1..self.out.len() - 1].to_string();
            }
        } else if !self.out.is_empty() {
            self.out.push(' ');
            self.out.push_str(&format!("{}={:?}", field.name(), value));
        } else {
            self.out.push_str(&format!("{}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.out = value.to_string();
        } else if !self.out.is_empty() {
            self.out.push(' ');
            self.out.push_str(&format!("{}={}", field.name(), value));
        } else {
            self.out.push_str(&format!("{}={}", field.name(), value));
        }
    }
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor { out: String::new() };
        event.record(&mut visitor);
        let meta = event.metadata();
        self.buf.push(LogLine {
            level: *meta.level(),
            target: meta.target().to_string(),
            message: visitor.out,
            at: chrono::Utc::now(),
        });
    }
}

/// Install the capture layer alongside the normal `fmt` subscriber.
/// Safe to call once per process; subsequent calls are no-ops and
/// return a fresh empty buffer (we have no way to recover the
/// already-registered one — callers should hold onto the result of
/// the first install).
pub fn install() -> LogBuffer {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let buf = LogBuffer::new();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hl_signer_desktop=info"));
    let capture = CaptureLayer { buf: buf.clone() };
    // Use try_init so the binary's earlier `init_tracing` doesn't make
    // the second registration panic; if a global subscriber is already
    // set we just return an empty buffer.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(capture)
        .try_init();
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_caps_at_max_lines() {
        let buf = LogBuffer::new();
        for i in 0..(MAX_LINES + 10) {
            buf.push(LogLine {
                level: Level::INFO,
                target: "t".into(),
                message: format!("{i}"),
                at: chrono::Utc::now(),
            });
        }
        assert_eq!(buf.len(), MAX_LINES);
        let snap = buf.snapshot();
        assert_eq!(snap.first().unwrap().message, "10");
        assert_eq!(snap.last().unwrap().message, format!("{}", MAX_LINES + 9));
    }

    #[test]
    fn snapshot_is_decoupled_from_writer() {
        let buf = LogBuffer::new();
        buf.push(LogLine {
            level: Level::INFO,
            target: "a".into(),
            message: "first".into(),
            at: chrono::Utc::now(),
        });
        let snap = buf.snapshot();
        buf.push(LogLine {
            level: Level::WARN,
            target: "b".into(),
            message: "second".into(),
            at: chrono::Utc::now(),
        });
        assert_eq!(snap.len(), 1);
        assert_eq!(buf.len(), 2);
    }
}
