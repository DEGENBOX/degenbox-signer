//! Wallet tab — keystore management.
//!
//! Shows where the keystore lives, whether it's currently unlocked,
//! the derived agent address, and offers actions: unlock, reveal
//! secret (with passphrase re-entry), delete (with confirm).

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::tui::app::{App, AppOutcome, ConfirmAction, ConnState, Modal};
use crate::tui::screens;
use crate::tui::widgets;

pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(0)])
        .split(area);
    render_summary(app, frame, chunks[0]);
    render_actions(app, frame, chunks[1]);
}

fn render_summary(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let unlocked = matches!(
        app.runtime
            .lock()
            .ok()
            .and_then(|r| r.conn)
            .unwrap_or(ConnState::Offline),
        ConnState::Ready | ConnState::Connecting | ConnState::Paused
    );
    let lock_state = if unlocked { "UNLOCKED" } else { "LOCKED" };
    let lock_style = if unlocked {
        app.theme.ok().add_modifier(Modifier::BOLD)
    } else {
        app.theme.warn().add_modifier(Modifier::BOLD)
    };

    let backend = if std::env::var_os("XDG_CONFIG_HOME").is_some() {
        "File (XDG_CONFIG_HOME)"
    } else {
        "File (~/.config/degenbox)"
    };

    let rows = vec![
        ("Keystore", app.ks_path.display().to_string()),
        ("Backend", backend.to_string()),
        (
            "Agent address",
            app.agent_address
                .clone()
                .unwrap_or_else(|| "(none — run setup)".into()),
        ),
        ("Network", format!("{:?}", app.cfg.network).to_lowercase()),
    ];
    let mut lines = widgets::kv_lines(&rows, app.theme);
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("State:  ", app.theme.muted()),
        Span::styled(lock_state, lock_style),
    ]));
    let p = Paragraph::new(lines).block(widgets::panel("Keystore", app.theme));
    frame.render_widget(p, area);
}

fn render_actions(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("[u]", app.theme.accent()),
            Span::raw("  Unlock keystore   "),
            Span::styled("[r]", app.theme.accent()),
            Span::raw("  Reveal secret key (requires passphrase)"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("[d]", app.theme.err()),
            Span::raw("  Delete keystore   "),
            Span::styled("[c]", app.theme.accent()),
            Span::raw("  Copy address to clipboard (terminal-dependent)"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "The keystore is encrypted with Argon2id + AES-256-GCM.",
                app.theme.muted(),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "The decrypted secret never leaves this process.",
                app.theme.muted(),
            ),
        ]),
    ];
    let p = Paragraph::new(lines).block(widgets::panel("Actions", app.theme));
    frame.render_widget(p, area);
}

pub fn handle_key(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> AppOutcome {
    match code {
        KeyCode::Char('u') => {
            screens::open_unlock_modal(app);
            AppOutcome::Continue
        }
        KeyCode::Char('r') => {
            app.modal = Some(Modal::RevealPhrase {
                input: String::new(),
                revealed: None,
                error: None,
            });
            AppOutcome::Continue
        }
        KeyCode::Char('d') => {
            app.modal = Some(Modal::Confirm {
                title: "Delete keystore?".into(),
                body: "This permanently deletes the encrypted keystore file. Make sure you have your backup phrase saved.".into(),
                on_yes: ConfirmAction::DeleteKeystore,
            });
            AppOutcome::Continue
        }
        _ => AppOutcome::Continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tui::app::{App, RuntimeState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn renders_wallet_tab() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::for_test(Config::default(), RuntimeState::new());
        terminal.draw(|f| render(&app, f, f.area())).unwrap();
        let buf = terminal.backend().buffer().clone();
        let dump = dump(&buf);
        assert!(dump.contains("Keystore"));
        assert!(dump.contains("Actions"));
        assert!(dump.contains("LOCKED") || dump.contains("UNLOCKED"));
    }

    #[test]
    fn r_opens_reveal_modal() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.tab = crate::tui::app::Tab::Wallet;
        app.on_key(KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(matches!(app.modal, Some(Modal::RevealPhrase { .. })));
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
