//! Status tab — the default landing screen.
//!
//! Three stacked sections:
//!
//! 1. Top:    key-value identity panel (signer key, HL account, conn state, user_id)
//! 2. Middle: live orders table (last 50 signed)
//! 3. Bottom: queue snapshot (pending, last drained, next preview)

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
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
    render_identity(app, frame, chunks[0]);
    render_orders(app, frame, chunks[1]);
    render_queue(app, frame, chunks[2]);
}

fn render_identity(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let conn = app
        .runtime
        .lock()
        .ok()
        .and_then(|r| r.conn)
        .unwrap_or(ConnState::Offline);
    let user_id = app
        .runtime
        .lock()
        .ok()
        .and_then(|r| r.user_id.clone())
        .unwrap_or_else(|| "(not registered)".to_string());
    let agent = app
        .agent_address
        .clone()
        .unwrap_or_else(|| "(no keystore)".to_string());
    let fp = fingerprint(&agent);
    let rows = vec![
        ("Signer key", fp),
        ("Agent address", agent),
        ("DegenBox user", user_id),
        ("Server", app.cfg.server_url.clone()),
        ("Connection", conn.label().to_string()),
    ];
    let lines = widgets::kv_lines(&rows, app.theme);
    let p = Paragraph::new(lines).block(widgets::panel("Identity", app.theme));
    frame.render_widget(p, area);
}

fn render_orders(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let rt = app.runtime.lock().ok();
    let orders: Vec<_> = rt
        .as_ref()
        .map(|r| r.orders.iter().rev().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if orders.is_empty() {
        frame.render_widget(
            widgets::empty_panel(
                "Recent orders",
                "No orders yet — waiting for the server to queue one.",
                app.theme,
            ),
            area,
        );
        return;
    }
    let header = Row::new(vec![
        Cell::from(Span::styled("time", app.theme.muted())),
        Cell::from(Span::styled("coin", app.theme.muted())),
        Cell::from(Span::styled("side", app.theme.muted())),
        Cell::from(Span::styled("size $", app.theme.muted())),
        Cell::from(Span::styled("status", app.theme.muted())),
        Cell::from(Span::styled("fill ms", app.theme.muted())),
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
            Row::new(vec![
                Cell::from(o.at.format("%H:%M:%S").to_string()),
                Cell::from(o.coin.clone()),
                Cell::from(side_cell(&o.side, app.theme)),
                Cell::from(o.size_usd.clone()),
                Cell::from(Span::styled(o.status.clone(), status_style)),
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
        Constraint::Length(10),
        Constraint::Length(8),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(widgets::panel("Recent orders", app.theme));
    frame.render_widget(table, area);
}

fn render_queue(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let rt = app.runtime.lock().ok();
    let queue = rt.as_ref().map(|r| r.queue.clone()).unwrap_or_default();
    let drained = queue
        .last_drained_at
        .map(|t| {
            let secs = (chrono::Utc::now() - t).num_seconds().max(0);
            format!("{}s ago", secs)
        })
        .unwrap_or_else(|| "never".to_string());
    let preview = queue
        .next_preview
        .clone()
        .unwrap_or_else(|| "(idle)".to_string());
    let lines = vec![
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

pub fn handle_key(_app: &mut App, _code: KeyCode, _mods: KeyModifiers) -> AppOutcome {
    // Status is read-only; the parent handles 'p' (pause) and tab nav.
    AppOutcome::Continue
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
        });
        rt.push_order(OrderRow {
            at: chrono::DateTime::from_timestamp(1_700_000_010, 0).unwrap(),
            coin: "ETH".into(),
            side: "sell".into(),
            size_usd: "800".into(),
            status: "err".into(),
            fill_ms: None,
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
        // Sanity: the buffer contains the things we put in it.
        let dump = buffer_to_string(&buf);
        assert!(dump.contains("Identity"));
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
