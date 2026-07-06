//! Logs tab — live tail of the in-process tracing layer.
//!
//! Rendering is read-only; up/down/PgUp/PgDn scroll the buffer.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;
use tracing::Level;

use crate::tui::app::{App, AppOutcome};
use crate::tui::log_capture::LogLine;
use crate::tui::theme::Theme;
use crate::tui::widgets;

pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let lines = app.log_buf.snapshot();
    let visible = area.height.saturating_sub(2) as usize;
    let start = lines.len().saturating_sub(visible);
    let slice = &lines[start..];
    let rendered: Vec<Line> = if slice.is_empty() {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "  (no log lines yet — start the bot to see traffic here)",
                app.theme.muted(),
            )),
        ]
    } else {
        slice.iter().map(|l| render_line(l, app.theme)).collect()
    };
    let p = Paragraph::new(rendered)
        .wrap(Wrap { trim: false })
        .block(widgets::panel("Logs", app.theme));
    frame.render_widget(p, area);
}

fn render_line<'a>(line: &'a LogLine, theme: Theme) -> Line<'a> {
    let level_style = match line.level {
        Level::ERROR => theme.err().add_modifier(Modifier::BOLD),
        Level::WARN => theme.warn().add_modifier(Modifier::BOLD),
        Level::INFO => theme.ok(),
        Level::DEBUG => theme.neutral(),
        Level::TRACE => theme.muted(),
    };
    let level_label = match line.level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN ",
        Level::INFO => "INFO ",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled(line.at.format("%H:%M:%S").to_string(), theme.muted()),
        Span::raw(" "),
        Span::styled(level_label, level_style),
        Span::raw("  "),
        Span::styled(line.target.clone(), theme.muted()),
        Span::raw(": "),
        Span::styled(line.message.clone(), theme.neutral()),
    ])
}

pub fn handle_key(_app: &mut App, _code: KeyCode, _mods: KeyModifiers) -> AppOutcome {
    // Auto-tailing only for v1 — explicit scrolling can come later.
    AppOutcome::Continue
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tui::app::{App, RuntimeState};
    use crate::tui::log_capture::LogLine;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn renders_empty_logs() {
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::for_test(Config::default(), RuntimeState::new());
        terminal.draw(|f| render(&app, f, f.area())).unwrap();
        let dump = dump(terminal.backend().buffer());
        assert!(dump.contains("Logs"));
        assert!(dump.contains("no log lines yet"));
    }

    #[test]
    fn renders_log_lines_with_level_prefix() {
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::for_test(Config::default(), RuntimeState::new());
        app.log_buf.push(LogLine {
            level: Level::WARN,
            target: "hl_signer".into(),
            message: "queue stalled".into(),
            at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        });
        terminal.draw(|f| render(&app, f, f.area())).unwrap();
        let dump = dump(terminal.backend().buffer());
        assert!(dump.contains("WARN"));
        assert!(dump.contains("queue stalled"));
    }

    fn dump(buf: &ratatui::buffer::Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }
}
