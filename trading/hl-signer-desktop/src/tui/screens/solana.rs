//! Solana tab — wallet, execution runtime + `:5829` web-bridge status,
//! recent activity. The execution semantics live in `crate::sol`; this
//! screen only projects the shared handles.

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::tui::app::{App, AppOutcome, Modal};
use crate::tui::widgets;

pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // wallet
            Constraint::Length(9), // execution runtime
            Constraint::Length(4), // web bridge (:5829)
            Constraint::Min(4),    // activity
        ])
        .split(area);
    render_wallet(app, frame, chunks[0]);
    render_runtime(app, frame, chunks[1]);
    render_bridge(app, frame, chunks[2]);
    render_activity(app, frame, chunks[3]);
}

fn render_wallet(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let sol = &app.sol;
    let mut lines = Vec::new();
    match (&sol.ks_path, &sol.pubkey) {
        (Some(path), pk) => {
            for (k, v) in [
                ("Keystore", path.display().to_string()),
                (
                    "Pubkey  ",
                    pk.clone().unwrap_or_else(|| "(unreadable)".into()),
                ),
            ] {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(k.to_string(), app.theme.muted()),
                    Span::raw("  "),
                    Span::styled(v, app.theme.neutral()),
                ]));
            }
            lines.push(Line::from(""));
            let (label, style) = if sol.is_unlocked() {
                ("UNLOCKED", app.theme.ok().add_modifier(Modifier::BOLD))
            } else {
                ("LOCKED", app.theme.warn().add_modifier(Modifier::BOLD))
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("State:  ", app.theme.muted()),
                Span::styled(label, style),
                Span::raw("    "),
                Span::styled(
                    "[u] unlock   [l] lock   [b] set copy budget",
                    app.theme.muted(),
                ),
            ]));
            if let Some(err) = &sol.unlock_error {
                lines.push(Line::from(Span::styled(
                    format!("  {err}"),
                    app.theme.err(),
                )));
            }
        }
        _ => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  No Solana keystore yet.",
                app.theme.warn().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                "  Run: hl-signer-desktop sol init   (or `sol import` for an existing wallet / extension export)",
                app.theme.muted(),
            )));
        }
    }
    let p = Paragraph::new(lines).block(widgets::panel("Solana wallet", app.theme));
    frame.render_widget(p, area);
}

fn render_runtime(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let st = app.sol.runtime.snapshot();
    let state_style = match st.state.as_str() {
        "ready" => app.theme.ok().add_modifier(Modifier::BOLD),
        "error" => app.theme.err().add_modifier(Modifier::BOLD),
        "offline" => app.theme.muted(),
        _ => app.theme.warn().add_modifier(Modifier::BOLD),
    };
    let budget = match st.copy_session_sol {
        Some(s) => format!(
            "{} ({:.4} / {s} SOL used)",
            if st.copy_armed { "ARMED" } else { "set" },
            st.copy_spent_sol
        ),
        None => "DISARMED — copy buys refused until a budget is set ([b])".into(),
    };
    let mut lines = vec![
        Line::from(vec![
            Span::raw("  "),
            Span::styled("State      ", app.theme.muted()),
            Span::styled(st.state.to_uppercase(), state_style),
            Span::raw("    "),
            Span::styled("user ", app.theme.muted()),
            Span::styled(
                st.user_id.unwrap_or_else(|| "-".into()),
                app.theme.neutral(),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Copy budget ", app.theme.muted()),
            Span::styled(
                budget,
                if st.copy_armed {
                    app.theme.ok()
                } else {
                    app.theme.warn()
                },
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Executed   ", app.theme.muted()),
            Span::styled(
                format!(
                    "{} sells · {} copies · {} failed",
                    st.sells_executed, st.copies_executed, st.events_failed
                ),
                app.theme.neutral(),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("Last event ", app.theme.muted()),
            Span::styled(
                st.last_event_at.unwrap_or_else(|| "-".into()),
                app.theme.muted(),
            ),
        ]),
    ];
    if let Some(err) = st.error {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("\u{26A0} ", app.theme.err()),
            Span::styled(err, app.theme.err()),
        ]));
    }
    let p = Paragraph::new(lines).block(widgets::panel(
        "Solana execution  (TP/SL sells + copytrade)",
        app.theme,
    ));
    frame.render_widget(p, area);
}

fn render_bridge(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let sol = &app.sol;
    let err = sol.daemon_error.lock().ok().and_then(|g| g.clone());
    let line = if let Some(e) = err {
        Line::from(vec![
            Span::raw("  "),
            Span::styled("\u{2717} ", app.theme.err()),
            Span::styled(e, app.theme.err()),
        ])
    } else if sol.daemon_alive.load(std::sync::atomic::Ordering::Relaxed) {
        Line::from(vec![
            Span::raw("  "),
            Span::styled("\u{25CF} ", app.theme.ok()),
            Span::styled(
                format!(
                    "serving 127.0.0.1:{} — the DegenBox web app can use this signer",
                    sol.daemon_port
                ),
                app.theme.neutral(),
            ),
        ])
    } else {
        Line::from(vec![
            Span::raw("  "),
            Span::styled("\u{25CB} ", app.theme.muted()),
            Span::styled(
                "not running — starts on unlock (web app detection + quote/swap)",
                app.theme.muted(),
            ),
        ])
    };
    let p = Paragraph::new(vec![line]).block(widgets::panel("Web bridge (:5829)", app.theme));
    frame.render_widget(p, area);
}

fn render_activity(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let acts: Vec<_> = app
        .sol
        .runtime
        .activity
        .lock()
        .map(|g| g.iter().rev().cloned().collect())
        .unwrap_or_default();
    if acts.is_empty() {
        let p = widgets::empty_panel(
            "Solana activity",
            "No events yet — TP/SL triggers and copy-trade commands land here.",
            app.theme,
        );
        frame.render_widget(p, area);
        return;
    }
    let header = Row::new(vec![
        Cell::from(Span::styled("time", app.theme.muted())),
        Cell::from(Span::styled("kind", app.theme.muted())),
        Cell::from(Span::styled("mint", app.theme.muted())),
        Cell::from(Span::styled("status", app.theme.muted())),
    ]);
    let rows: Vec<Row> = acts
        .iter()
        .map(|a| {
            let status_style = match a.status.as_str() {
                "submitted" => app.theme.ok(),
                "failed" => app.theme.err(),
                _ => app.theme.muted(),
            };
            Row::new(vec![
                Cell::from(a.at.format("%H:%M:%S").to_string()),
                Cell::from(a.kind.clone()),
                Cell::from(short_mint(&a.mint)),
                Cell::from(Span::styled(a.status.clone(), status_style)),
            ])
        })
        .collect();
    let t = Table::new(
        rows,
        [
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Min(10),
        ],
    )
    .header(header)
    .block(widgets::panel("Solana activity", app.theme));
    frame.render_widget(t, area);
}

fn short_mint(m: &str) -> String {
    if m.len() <= 12 {
        m.to_string()
    } else {
        format!("{}…{}", &m[..6], &m[m.len() - 4..])
    }
}

pub fn handle_key(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> AppOutcome {
    match code {
        KeyCode::Char('u') if app.sol.has_keystore() && !app.sol.is_unlocked() => {
            app.modal = Some(Modal::SolUnlock {
                input: String::new(),
                error: None,
            });
            AppOutcome::Continue
        }
        KeyCode::Char('l') if app.sol.is_unlocked() => {
            app.sol.lock();
            AppOutcome::Continue
        }
        KeyCode::Char('b') => {
            let current = crate::sol::config::SolConfig::load_or_default()
                .copy_session_sol
                .map(|s| s.to_string())
                .unwrap_or_default();
            app.modal = Some(Modal::SolBudget {
                input: current,
                error: None,
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
    use crate::tui::app::{App, RuntimeState, Tab};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn renders_solana_tab_without_keystore() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::for_test(Config::default(), RuntimeState::new());
        terminal.draw(|f| render(&app, f, f.area())).unwrap();
        let mut dump = String::new();
        let buf = terminal.backend().buffer().clone();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                dump.push_str(buf[(x, y)].symbol());
            }
            dump.push('\n');
        }
        assert!(dump.contains("Solana wallet"));
        assert!(dump.contains("sol init"));
        assert!(dump.contains("Web bridge"));
        assert!(dump.contains("Solana activity"));
    }

    #[test]
    fn budget_key_opens_modal() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.tab = Tab::Solana;
        app.on_key(KeyCode::Char('b'), KeyModifiers::NONE);
        assert!(matches!(app.modal, Some(Modal::SolBudget { .. })));
    }
}
