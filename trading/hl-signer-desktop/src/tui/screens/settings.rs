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
];
const NUM_ROWS: usize = 4;

pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let s = &app.settings;
    let row_values: [String; NUM_ROWS] = [
        s.poll_secs.clone(),
        s.log_level.clone(),
        bool_str(s.paper_mode),
        bool_str(s.auto_update),
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
            let mut style = app
                .theme
                .neutral()
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            if s.is_editing {
                style = style.add_modifier(Modifier::REVERSED);
            }
            style
        } else {
            app.theme.neutral()
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(arrow.to_string(), arrow_style),
            Span::raw(" "),
            Span::styled(format!("{label:max_label$}"), app.theme.muted()),
            Span::raw("   "),
            Span::styled(
                if focused && s.is_editing {
                    format!("{}█", row_values[i])
                } else {
                    row_values[i].clone()
                },
                val_style,
            ),
        ]));
    }
    lines.push(Line::from(""));
    // Paper-mode is a REAL dry-run: when on, the signer reports `paper`
    // and NEVER POSTs to HL. Spell that out so it can't be mistaken for a
    // cosmetic toggle.
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            if s.paper_mode {
                "Paper mode ON — dry run: instructions are acked but NOT sent to HL."
            } else {
                "Paper mode OFF — LIVE: instructions are signed and sent to Hyperliquid."
            },
            if s.paper_mode {
                app.theme.warn()
            } else {
                app.theme.muted()
            },
        ),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            if s.is_editing {
                "Type to edit, [Enter] or [Esc] to confirm."
            } else {
                "Up/Down to focus, [Enter] to edit values, [Space] to toggle, [s] to save."
            },
            app.theme.muted(),
        ),
    ]));

    // Add space and then the "Create New Client" button
    lines.push(Line::from(""));
    let btn_focused = s.focus_row == NUM_ROWS;
    let btn_style = if btn_focused {
        app.theme
            .accent()
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        app.theme.neutral().add_modifier(Modifier::BOLD)
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            if btn_focused {
                "> [ Create New Client ]"
            } else {
                "  [ Create New Client ]"
            },
            btn_style,
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

    if app.settings.is_editing {
        match code {
            KeyCode::Enter | KeyCode::Esc => {
                app.settings.is_editing = false;
                return AppOutcome::Continue;
            }
            KeyCode::Backspace => {
                mutate_text(app, row, |s| {
                    s.pop();
                });
                return AppOutcome::Continue;
            }
            KeyCode::Char(c) => {
                mutate_text(app, row, |s| s.push(c));
                return AppOutcome::Continue;
            }
            _ => return AppOutcome::Continue,
        }
    }

    match code {
        KeyCode::Up => {
            if app.settings.focus_row > 0 {
                app.settings.focus_row -= 1;
            }
            AppOutcome::Continue
        }
        KeyCode::Down => {
            if app.settings.focus_row < NUM_ROWS {
                app.settings.focus_row += 1;
            }
            AppOutcome::Continue
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
            if row < NUM_ROWS {
                toggle(app, row);
            }
            AppOutcome::Continue
        }
        KeyCode::Enter => {
            if row == NUM_ROWS {
                return AppOutcome::RunWizard;
            } else if row == 0 || row == 1 {
                // Edit poll interval or log level
                app.settings.is_editing = true;
            }
            AppOutcome::Continue
        }
        KeyCode::Char('s') => {
            // Fold the editable draft back into the active bot's config so
            // the per-bot poll cadence actually persists (it used to be a
            // dead "3"). paper_mode is applied on the next unlock/restart.
            let poll = app
                .settings
                .poll_secs
                .parse::<u64>()
                .ok()
                .filter(|n| *n >= 1);
            let outcome = if let Some(bot) = app.bots.get_mut(app.active_bot_idx) {
                if let Some(p) = poll {
                    bot.cfg.poll_secs = p;
                }
                match crate::config::save(&bot.dir.join("hl-config.json"), &bot.cfg) {
                    Ok(()) => Ok(()),
                    Err(e) => Err(e.to_string()),
                }
            } else {
                Err("__no_client__".into())
            };
            match outcome {
                Ok(()) => {
                    app.modal = Some(crate::tui::app::Modal::Message {
                        title: "Saved".into(),
                        body: "Settings written. Poll cadence applies now; \
                               paper-mode toggles on the next unlock/restart."
                            .into(),
                    });
                }
                Err(ref e) if e == "__no_client__" => {
                    app.modal = Some(crate::tui::app::Modal::Message {
                        title: "No client selected".into(),
                        body: "Please create a new client first.".into(),
                    });
                }
                Err(e) => {
                    app.modal = Some(crate::tui::app::Modal::Message {
                        title: "Save failed".into(),
                        body: e,
                    });
                }
            }
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
    fn arrow_down_moves_focus_until_create_client_button() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.tab = Tab::Settings;
        for _ in 0..10 {
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
        }
        // The last focusable row is the "Create New Client" button at index
        // `NUM_ROWS` (one past the editable settings rows), so Down clamps
        // there — not at `NUM_ROWS - 1`.
        assert_eq!(app.settings.focus_row, NUM_ROWS);
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
