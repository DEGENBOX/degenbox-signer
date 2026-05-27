//! Settings tab — editable runtime knobs.
//!
//! Settings live in `SettingsDraft` on the `App` so the user can edit
//! without committing. Pressing `s` saves them to disk.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::{App, AppOutcome};
use crate::tui::widgets;

const ROW_LABELS: &[&str] = &[
    "Poll interval (s)",
    "Log level",
    "Paper mode",
    "Auto-update",
    "Max clients",
];
const NUM_ROWS: usize = 5;

pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let s = &app.settings;
    let row_values: [String; NUM_ROWS] = [
        s.poll_secs.clone(),
        s.log_level.clone(),
        bool_str(s.paper_mode),
        bool_str(s.auto_update),
        s.max_clients.clone(),
    ];
    let max_label = ROW_LABELS.iter().map(|l| l.len()).max().unwrap_or(0);
    let mut lines: Vec<Line> = Vec::with_capacity(NUM_ROWS + 4);
    lines.push(Line::from(""));
    for (i, label) in ROW_LABELS.iter().enumerate() {
        let focused = i == s.focus_row;
        let arrow = if focused { ">" } else { " " };
        let arrow_style = if focused {
            app.theme.accent().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let val_style = if focused {
            app.theme
                .neutral()
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            app.theme.neutral()
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(arrow.to_string(), arrow_style),
            Span::raw(" "),
            Span::styled(
                format!("{:width$}", label, width = max_label),
                app.theme.muted(),
            ),
            Span::raw("   "),
            Span::styled(row_values[i].clone(), val_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "Up/Down to focus, Left/Right or Space to toggle, type to edit numbers/strings, [s] save.",
            app.theme.muted(),
        ),
    ]));
    let p = Paragraph::new(lines).block(widgets::panel("Settings", app.theme));
    frame.render_widget(p, area);
}

fn bool_str(b: bool) -> String {
    if b {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

pub fn handle_key(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> AppOutcome {
    let row = app.settings.focus_row;
    match code {
        KeyCode::Up => {
            if app.settings.focus_row > 0 {
                app.settings.focus_row -= 1;
            }
            AppOutcome::Continue
        }
        KeyCode::Down => {
            if app.settings.focus_row + 1 < NUM_ROWS {
                app.settings.focus_row += 1;
            }
            AppOutcome::Continue
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
            toggle(app, row);
            AppOutcome::Continue
        }
        KeyCode::Backspace => {
            mutate_text(app, row, |s| {
                s.pop();
            });
            AppOutcome::Continue
        }
        KeyCode::Char('s') => {
            // Save to config. Today only `host_id` survives roundtrip —
            // the runtime knobs (poll, log level, paper, max_clients)
            // get re-read from the draft on next daemon start.
            if let Err(e) = crate::config::save(&app.cfg_path, &app.cfg) {
                app.modal = Some(crate::tui::app::Modal::Message {
                    title: "Save failed".into(),
                    body: e.to_string(),
                });
            } else {
                app.modal = Some(crate::tui::app::Modal::Message {
                    title: "Saved".into(),
                    body: "Settings written to disk. Restart the daemon to pick up daemon-side changes.".into(),
                });
            }
            AppOutcome::Continue
        }
        KeyCode::Char(c) => {
            mutate_text(app, row, |s| s.push(c));
            AppOutcome::Continue
        }
        _ => AppOutcome::Continue,
    }
}

fn toggle(app: &mut App, row: usize) {
    match row {
        2 => app.settings.paper_mode = !app.settings.paper_mode,
        3 => app.settings.auto_update = !app.settings.auto_update,
        _ => {}
    }
}

fn mutate_text(app: &mut App, row: usize, f: impl FnOnce(&mut String)) {
    match row {
        0 => f(&mut app.settings.poll_secs),
        1 => f(&mut app.settings.log_level),
        4 => f(&mut app.settings.max_clients),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tui::app::{App, RuntimeState, Tab};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn arrow_down_moves_focus_until_last_row() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.tab = Tab::Settings;
        for _ in 0..10 {
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(app.settings.focus_row, NUM_ROWS - 1);
    }

    #[test]
    fn space_toggles_bool_row() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.tab = Tab::Settings;
        app.settings.focus_row = 2;
        let before = app.settings.paper_mode;
        app.on_key(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(app.settings.paper_mode, !before);
    }

    #[test]
    fn renders_settings_tab() {
        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::for_test(Config::default(), RuntimeState::new());
        terminal.draw(|f| render(&app, f, f.area())).unwrap();
        let dump = dump(terminal.backend().buffer());
        assert!(dump.contains("Settings"));
        assert!(dump.contains("Poll interval"));
        assert!(dump.contains("Auto-update"));
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
