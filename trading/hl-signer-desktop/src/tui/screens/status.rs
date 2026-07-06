//! Status tab — the default landing screen.
//!
//! Three stacked sections:
//!
//! 1. Top:    key-value identity panel (signer key, HL account, conn state, user_id)
//! 2. Middle: live orders table (last 50 signed)
//! 3. Bottom: queue snapshot (pending, last drained, next preview)

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::tui::app::{App, AppOutcome, ConnState};
use crate::tui::widgets;

pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(5),
            Constraint::Length(5),
        ])
        .split(area);
    render_bot_list(app, frame, chunks[0]);
    render_orders(app, frame, chunks[1]);
    render_queue(app, frame, chunks[2]);
}

fn render_bot_list(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let header = Row::new(vec![
        Cell::from(Span::styled("Client", app.theme.muted())),
        Cell::from(Span::styled("Status", app.theme.muted())),
        Cell::from(Span::styled("Account $", app.theme.muted())),
        Cell::from(Span::styled("Poll", app.theme.muted())),
        Cell::from(Span::styled("Net", app.theme.muted())),
        Cell::from(Span::styled("Agent", app.theme.muted())),
    ]);

    let mut rows = Vec::new();
    let tick = (app.uptime().as_millis() / 120) as u64;

    for (i, bot) in app.bots.iter().enumerate() {
        let is_selected = i == app.active_bot_idx;
        let rt = bot.runtime.lock().ok();
        let conn = rt
            .as_ref()
            .and_then(|r| r.conn)
            .unwrap_or(ConnState::Offline);
        let paper = rt.as_ref().map(|r| r.paper_mode).unwrap_or(false);
        let acct_value = rt
            .as_ref()
            .and_then(|r| r.balance.account_value_usd.clone());
        let last_poll = rt.as_ref().and_then(|r| r.last_poll_at);

        let (glyph, status_style, label) = match conn {
            ConnState::Connecting => (
                crate::tui::logo::spinner(tick),
                app.theme.warn().add_modifier(Modifier::BOLD),
                "CONNECTING",
            ),
            ConnState::Ready => (
                crate::tui::logo::pulse(tick),
                app.theme.ok().add_modifier(Modifier::BOLD),
                "ONLINE",
            ),
            ConnState::Error => (
                "\u{2717}",
                app.theme.err().add_modifier(Modifier::BOLD),
                "ERROR",
            ),
            ConnState::Paused => (
                "\u{23F8}",
                app.theme.paused().add_modifier(Modifier::BOLD),
                "PAUSED",
            ),
            ConnState::Offline => ("\u{25CB}", app.theme.muted(), "OFFLINE"),
        };

        let addr = bot.agent_address.clone().unwrap_or_else(|| "-".into());
        let addr_short = fingerprint(&addr);

        let style = if is_selected {
            app.theme
                .neutral()
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            app.theme.neutral()
        };

        // Name cell carries a PAPER badge so a dry-run client is never
        // mistaken for live.
        let name_label = if paper {
            format!(
                "{} {} [PAPER]",
                if is_selected { ">" } else { " " },
                bot.name
            )
        } else {
            format!("{} {}", if is_selected { ">" } else { " " }, bot.name)
        };

        let acct_cell = acct_value
            .map(|v| format!("${v}"))
            .unwrap_or_else(|| "-".into());
        let poll_cell = match last_poll {
            Some(t) => format!("{}s", (chrono::Utc::now() - t).num_seconds().max(0)),
            None => "-".into(),
        };

        rows.push(Row::new(vec![
            Cell::from(Span::styled(name_label, style)),
            Cell::from(Span::styled(format!("{glyph} {label}"), status_style)),
            Cell::from(Span::styled(acct_cell, app.theme.neutral())),
            Cell::from(Span::styled(poll_cell, app.theme.muted())),
            Cell::from(Span::styled(
                format!("{:?}", bot.cfg.network).to_lowercase(),
                app.theme.muted(),
            )),
            Cell::from(Span::styled(addr_short, app.theme.muted())),
        ]));
    }

    if rows.is_empty() {
        rows.push(Row::new(vec![Cell::from(Span::styled(
            "No clients configured — go to Settings ▸ Create New Client.",
            app.theme.warn(),
        ))]));
    }

    let t = Table::new(
        rows,
        [
            Constraint::Length(22),
            Constraint::Length(14),
            Constraint::Length(12),
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Min(14),
        ],
    )
    .header(header)
    .block(widgets::panel("Clients  (↑/↓ select · p pause)", app.theme));

    frame.render_widget(t, area);
}

fn render_orders(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let rt = app
        .bots
        .get(app.active_bot_idx)
        .and_then(|b| b.runtime.lock().ok());
    let orders: Vec<_> = rt
        .as_ref()
        .map(|r| r.orders.iter().rev().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if orders.is_empty() {
        // Branded idle splash — show the DegenBox logo until the first order
        // flows in, so the dashboard has presence instead of an empty panel.
        let mut lines = vec![Line::from("")];
        for l in crate::tui::logo::LOGO {
            lines.push(Line::from(Span::styled(
                *l,
                app.theme.accent().add_modifier(Modifier::BOLD),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            crate::tui::logo::TAGLINE,
            app.theme.muted(),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "No orders yet — the bot is watching for signals.",
            app.theme.muted(),
        )));
        let p = Paragraph::new(lines)
            .block(widgets::panel("Recent orders", app.theme))
            .alignment(ratatui::layout::Alignment::Center);
        frame.render_widget(p, area);
        return;
    }
    let header = Row::new(vec![
        Cell::from(Span::styled("time", app.theme.muted())),
        Cell::from(Span::styled("coin", app.theme.muted())),
        Cell::from(Span::styled("side", app.theme.muted())),
        Cell::from(Span::styled("size $", app.theme.muted())),
        Cell::from(Span::styled("status", app.theme.muted())),
        Cell::from(Span::styled("pnl", app.theme.muted())),
        Cell::from(Span::styled("ms", app.theme.muted())),
    ])
    .height(1);
    let rows: Vec<Row> = orders
        .iter()
        .map(|o| {
            let status_style =
                if o.status.eq_ignore_ascii_case("ok") || o.status.eq_ignore_ascii_case("filled") {
                    app.theme.ok()
                } else if o.status.eq_ignore_ascii_case("queued")
                    || o.status.eq_ignore_ascii_case("pending")
                {
                    app.theme.warn()
                } else if o.status.eq_ignore_ascii_case("err")
                    || o.status.eq_ignore_ascii_case("error")
                    || o.status.eq_ignore_ascii_case("failed")
                {
                    app.theme.err()
                } else {
                    app.theme.neutral()
                };
            let pnl_cell = match &o.pnl {
                Some(p) => {
                    let neg = p.trim_start().starts_with('-');
                    Span::styled(
                        p.clone(),
                        if neg { app.theme.err() } else { app.theme.ok() },
                    )
                }
                None => Span::styled("-".to_string(), app.theme.muted()),
            };
            Row::new(vec![
                Cell::from(o.at.format("%H:%M:%S").to_string()),
                Cell::from(o.coin.clone()),
                Cell::from(side_cell(&o.side, app.theme)),
                Cell::from(o.size_usd.clone()),
                Cell::from(Span::styled(o.status.clone(), status_style)),
                Cell::from(pnl_cell),
                Cell::from(
                    o.fill_ms
                        .map(|ms| format!("{ms}"))
                        .unwrap_or_else(|| "-".to_string()),
                ),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(6),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(widgets::panel("Recent orders", app.theme));
    frame.render_widget(table, area);
}

fn render_queue(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let rt = app
        .bots
        .get(app.active_bot_idx)
        .and_then(|b| b.runtime.lock().ok());
    let queue = rt.as_ref().map(|r| r.queue.clone()).unwrap_or_default();
    let last_error = rt.as_ref().and_then(|r| r.error.clone());
    let drained = queue
        .last_drained_at
        .map(|t| {
            let secs = (chrono::Utc::now() - t).num_seconds().max(0);
            format!("{secs}s ago")
        })
        .unwrap_or_else(|| "never".to_string());
    let preview = queue
        .next_preview
        .clone()
        .unwrap_or_else(|| "(idle)".to_string());
    let mut lines = vec![
        Line::from(vec![
            Span::raw("  "),
            Span::styled("pending: ", app.theme.muted()),
            Span::styled(
                queue.pending.to_string(),
                if queue.pending > 0 {
                    app.theme.warn().add_modifier(Modifier::BOLD)
                } else {
                    app.theme.ok()
                },
            ),
            Span::raw("    "),
            Span::styled("last drained: ", app.theme.muted()),
            Span::styled(drained, app.theme.neutral()),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("next: ", app.theme.muted()),
            Span::styled(preview, app.theme.neutral()),
        ]),
    ];
    // Surface the most recent client error so failures aren't silent.
    if let Some(err) = last_error {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("\u{26A0} ", app.theme.err()),
            Span::styled(err, app.theme.err()),
        ]));
    }
    let p = Paragraph::new(lines).block(widgets::panel("Queue", app.theme));
    frame.render_widget(p, area);
}

fn side_cell(side: &str, theme: crate::tui::theme::Theme) -> Span<'_> {
    if side.eq_ignore_ascii_case("buy") || side.eq_ignore_ascii_case("long") {
        Span::styled(side.to_string(), theme.ok())
    } else if side.eq_ignore_ascii_case("sell") || side.eq_ignore_ascii_case("short") {
        Span::styled(side.to_string(), theme.err())
    } else {
        Span::styled(side.to_string(), theme.neutral())
    }
}

/// First 6 + last 4 hex chars of an address, separated by `…`.
fn fingerprint(addr: &str) -> String {
    let trimmed = addr.trim_start_matches("0x");
    if trimmed.len() < 12 {
        return addr.to_string();
    }
    let head = &trimmed[..6];
    let tail = &trimmed[trimmed.len() - 4..];
    format!("0x{head}\u{2026}{tail}")
}

pub fn handle_key(app: &mut App, code: KeyCode, _mods: KeyModifiers) -> AppOutcome {
    match code {
        KeyCode::Up => {
            if app.active_bot_idx > 0 {
                app.active_bot_idx -= 1;
            }
            AppOutcome::Continue
        }
        KeyCode::Down => {
            if app.active_bot_idx + 1 < app.bots.len() {
                app.active_bot_idx += 1;
            }
            AppOutcome::Continue
        }
        KeyCode::Char('p') => {
            // Real pause: flip the shared atomic the daemon reads.
            app.toggle_pause_active();
            AppOutcome::Continue
        }
        _ => AppOutcome::Continue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tui::app::{App, OrderRow, RuntimeState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn snapshot_app() -> App {
        let mut rt = RuntimeState::new();
        rt.conn = Some(ConnState::Ready);
        rt.user_id = Some("user-abc".into());
        rt.push_order(OrderRow {
            at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            coin: "BTC".into(),
            side: "buy".into(),
            size_usd: "1200".into(),
            status: "filled".into(),
            fill_ms: Some(143),
            pnl: None,
        });
        rt.push_order(OrderRow {
            at: chrono::DateTime::from_timestamp(1_700_000_010, 0).unwrap(),
            coin: "ETH".into(),
            side: "sell".into(),
            size_usd: "800".into(),
            status: "err".into(),
            fill_ms: None,
            pnl: Some("-12.5".into()),
        });
        rt.queue.pending = 2;
        rt.queue.next_preview = Some("BTC buy 100".into());
        App::for_test(Config::default(), rt)
    }

    #[test]
    fn fingerprint_shortens_addresses() {
        assert_eq!(
            fingerprint("0x1234567890abcdef1234567890abcdef12345678"),
            "0x123456\u{2026}5678"
        );
        // Too short to fingerprint — return as-is.
        assert_eq!(fingerprint("0xdead"), "0xdead");
    }

    #[test]
    fn renders_status_screen_without_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = snapshot_app();
        terminal.draw(|f| render(&app, f, f.area())).unwrap();
        let buf = terminal.backend().buffer().clone();
        // Sanity: the buffer contains the things we put in it. The post-hub
        // renderer surfaces a per-client "Clients" panel (was "Identity"
        // in the single-runtime build), plus the orders + queue panels.
        let dump = buffer_to_string(&buf);
        assert!(dump.contains("Clients"), "client list panel present");
        assert!(dump.contains("Recent orders"));
        assert!(dump.contains("Queue"));
        assert!(dump.contains("BTC"));
    }

    fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
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
