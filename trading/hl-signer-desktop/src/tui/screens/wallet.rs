//! Wallet tab — keystore management.
//!
//! Shows where the keystore lives, whether it's currently unlocked,
//! the derived agent address, and offers actions: unlock, reveal
//! secret (with passphrase re-entry), delete (with confirm).

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::tui::app::{App, AppOutcome, ConfirmAction, ConnState, Modal};
use crate::tui::screens;
use crate::tui::widgets;

pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(10), // keystore summary
            Constraint::Length(7),  // balance
            Constraint::Min(0),     // positions + actions
        ])
        .split(area);
    render_summary(app, frame, chunks[0]);
    render_balance(app, frame, chunks[1]);
    render_positions_and_actions(app, frame, chunks[2]);
}

/// Live per-client balance, fetched off the MASTER account (the agent
/// always reads $0). Shows an explicit "not paired" / error message
/// rather than a blank or misleading $0.
fn render_balance(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let bot = app.bots.get(app.active_bot_idx);
    let paired = bot.and_then(|b| b.cfg.account_address.clone());
    let bal = bot
        .and_then(|b| b.runtime.lock().ok())
        .map(|r| r.balance.clone())
        .unwrap_or_default();

    let mut lines = Vec::new();
    match (&paired, &bal.error, &bal.account_value_usd) {
        (None, _, _) => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Account not paired — balances unavailable.",
                app.theme.warn().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                "  Run: register --account 0x… (your HL master wallet).",
                app.theme.muted(),
            )));
        }
        (Some(master), _, Some(value)) => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Account value  ", app.theme.muted()),
                Span::styled(format!("${value}"), app.theme.emphasis()),
                Span::raw("    "),
                Span::styled("free  ", app.theme.muted()),
                Span::styled(
                    bal.withdrawable_usd
                        .as_ref()
                        .map(|w| format!("${w}"))
                        .unwrap_or_else(|| "-".into()),
                    app.theme.neutral(),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Master  ", app.theme.muted()),
                Span::styled(short_addr(master), app.theme.neutral()),
                Span::raw("   "),
                Span::styled(
                    bal.fetched_at
                        .map(|t| format!("updated {}s ago", age_secs(t)))
                        .unwrap_or_else(|| "fetching…".into()),
                    app.theme.muted(),
                ),
            ]));
        }
        (Some(master), Some(err), _) => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Master  ", app.theme.muted()),
                Span::styled(short_addr(master), app.theme.neutral()),
            ]));
            lines.push(Line::from(Span::styled(
                format!("  balance fetch failed: {err}"),
                app.theme.err(),
            )));
        }
        (Some(_), None, None) => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Fetching balance from Hyperliquid…",
                app.theme.muted(),
            )));
        }
    }
    let p = Paragraph::new(lines).block(widgets::panel("Balance", app.theme));
    frame.render_widget(p, area);
}

fn render_positions_and_actions(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(4), Constraint::Length(8)])
        .split(area);
    render_positions(app, frame, chunks[0]);
    render_actions(app, frame, chunks[1]);
}

fn render_positions(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let positions = app
        .bots
        .get(app.active_bot_idx)
        .and_then(|b| b.runtime.lock().ok())
        .map(|r| r.balance.positions.clone())
        .unwrap_or_default();
    if positions.is_empty() {
        let p = widgets::empty_panel("Open positions", "No open positions.", app.theme);
        frame.render_widget(p, area);
        return;
    }
    let header = Row::new(vec![
        Cell::from(Span::styled("coin", app.theme.muted())),
        Cell::from(Span::styled("side", app.theme.muted())),
        Cell::from(Span::styled("size", app.theme.muted())),
    ]);
    let rows: Vec<Row> = positions
        .iter()
        .map(|p| {
            let side_style = if p.side == "short" {
                app.theme.err()
            } else {
                app.theme.ok()
            };
            Row::new(vec![
                Cell::from(p.coin.clone()),
                Cell::from(Span::styled(p.side.clone(), side_style)),
                Cell::from(p.szi.clone()),
            ])
        })
        .collect();
    let t = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(widgets::panel("Open positions", app.theme));
    frame.render_widget(t, area);
}

/// 0x1234…5678 short form for an address.
fn short_addr(addr: &str) -> String {
    if addr.len() > 12 {
        format!("{}…{}", &addr[..6], &addr[addr.len() - 4..])
    } else {
        addr.to_string()
    }
}

fn age_secs(t: chrono::DateTime<chrono::Utc>) -> i64 {
    (chrono::Utc::now() - t).num_seconds().max(0)
}

fn render_summary(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let bot = app.bots.get(app.active_bot_idx);
    let unlocked = matches!(
        bot.and_then(|b| b.runtime.lock().ok())
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

    let ks_path = bot
        .map(|b| b.ks_path.display().to_string())
        .unwrap_or_else(|| "(no client)".into());
    let agent_addr = bot
        .and_then(|b| b.agent_address.clone())
        .unwrap_or_else(|| "(none — run setup)".into());
    let network = bot
        .map(|b| format!("{:?}", b.cfg.network).to_lowercase())
        .unwrap_or_else(|| "-".into());

    let rows = vec![
        ("Keystore", ks_path),
        ("Backend", backend.to_string()),
        ("Agent address", agent_addr),
        ("Network", network),
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
