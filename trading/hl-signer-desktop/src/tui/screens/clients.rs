//! Clients tab — the terminal equivalent of the desktop app's fleet
//! table: every vault wallet (chain, label, address, primary, pause,
//! live runtime state) merged with the gateway's
//! `GET /api/trading/clients` registry. Mutations go through the same
//! vault helpers the headless `clients …` commands use.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyModifiers};
use degenbox_signer_core::{WalletChain, WalletEntry};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::clients::{ClientInfo, GatewayClient};
use crate::tui::app::{App, AppOutcome, ConfirmAction, Modal, VaultRole};
use crate::tui::widgets;

/// Result of the async gateway fetch.
#[derive(Debug, Clone)]
pub enum GatewayFetch {
    NotStarted,
    Pending,
    /// `None` = endpoint absent (404/405) — local-only view.
    Done(Option<Vec<GatewayClient>>),
    Failed(String),
}

pub struct ClientsPanel {
    pub sel: usize,
    pub entries: Vec<WalletEntry>,
    pub primary_sol: Option<String>,
    pub primary_hl: Option<String>,
    pub vault_dir: Option<PathBuf>,
    pub gateway: Arc<Mutex<GatewayFetch>>,
    /// One-line status (last action / hint), shown under the table.
    pub notice: Option<String>,
}

impl ClientsPanel {
    pub fn bootstrap() -> Self {
        let mut p = Self {
            sel: 0,
            entries: Vec::new(),
            primary_sol: None,
            primary_hl: None,
            vault_dir: crate::clients::vault_dir().ok(),
            gateway: Arc::new(Mutex::new(GatewayFetch::NotStarted)),
            notice: None,
        };
        p.reload();
        p
    }

    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            sel: 0,
            entries: Vec::new(),
            primary_sol: None,
            primary_hl: None,
            vault_dir: None,
            gateway: Arc::new(Mutex::new(GatewayFetch::NotStarted)),
            notice: None,
        }
    }

    /// Re-read the vault manifest (cheap — metadata only, no password).
    pub fn reload(&mut self) {
        match crate::clients::open_vault() {
            Ok(Some(v)) => {
                self.primary_sol = v.primary(WalletChain::Sol).map(|w| w.id.clone());
                self.primary_hl = v.primary(WalletChain::Hl).map(|w| w.id.clone());
                self.entries = v.wallets().to_vec();
            }
            _ => {
                self.entries.clear();
                self.primary_sol = None;
                self.primary_hl = None;
            }
        }
        if self.sel >= self.entries.len() {
            self.sel = self.entries.len().saturating_sub(1);
        }
    }

    pub fn selected(&self) -> Option<&WalletEntry> {
        self.entries.get(self.sel)
    }

    /// Kick off (or re-kick) the gateway merge in the background.
    pub fn refresh_gateway(&mut self) {
        let slot = self.gateway.clone();
        if let Ok(mut g) = slot.lock() {
            *g = GatewayFetch::Pending;
        }
        let Some(auth) = crate::clients::gateway_auth() else {
            if let Ok(mut g) = self.gateway.lock() {
                *g = GatewayFetch::Failed(
                    "not connected — run `hl-signer-desktop login` or pair via `register`".into(),
                );
            }
            return;
        };
        tokio::spawn(async move {
            let res = crate::clients::fetch_gateway_clients(&auth).await;
            if let Ok(mut g) = slot.lock() {
                *g = match res {
                    Ok(rows) => GatewayFetch::Done(rows),
                    Err(e) => GatewayFetch::Failed(e),
                };
            }
        });
    }

    /// Merged rows for rendering (and for the gateway column).
    pub fn merged(&self) -> Vec<ClientInfo> {
        let gw = self
            .gateway
            .lock()
            .ok()
            .map(|g| g.clone())
            .unwrap_or(GatewayFetch::NotStarted);
        let rows = match &gw {
            GatewayFetch::Done(rows) => rows.as_deref(),
            _ => None,
        };
        crate::clients::merge_clients(
            &self.entries,
            self.primary_sol.as_deref(),
            self.primary_hl.as_deref(),
            rows,
        )
    }
}

fn short_addr(a: &str) -> String {
    if a.len() <= 14 {
        a.to_string()
    } else {
        format!("{}…{}", &a[..6], &a[a.len() - 6..])
    }
}

/// Live runtime state for a vault entry, projected from the TUI's bot
/// handles (HL) / Sol panel — the in-process equivalent of the app's
/// `runtime_state_for`.
fn runtime_state_for(app: &App, entry: &WalletEntry) -> String {
    match entry.chain {
        WalletChain::Hl => {
            let Some(bot) = app
                .bots
                .iter()
                .find(|b| b.vault_id.as_deref() == Some(entry.id.as_str()))
            else {
                return "locked".into();
            };
            if !bot.is_running() {
                return "locked".into();
            }
            let conn = bot
                .runtime
                .lock()
                .ok()
                .and_then(|r| r.conn)
                .map(|c| c.label().to_lowercase())
                .unwrap_or_else(|| "offline".into());
            match bot.vault_role {
                Some(VaultRole::Standby) => format!("secondary:{conn}"),
                _ => format!("executor:{conn}"),
            }
        }
        WalletChain::Sol => {
            let is_primary = Some(&entry.id) == app.clients.primary_sol.as_ref();
            if !is_primary {
                return "standby".into();
            }
            if app.sol.is_unlocked() {
                format!("executor:{}", app.sol.runtime.snapshot().state)
            } else {
                "locked".into()
            }
        }
    }
}

pub fn render(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // fleet table
            Constraint::Length(4), // gateway / vault status
            Constraint::Length(2), // key hints
        ])
        .split(area);
    render_table(app, frame, chunks[0]);
    render_status(app, frame, chunks[1]);
    render_hints(app, frame, chunks[2]);
}

fn render_table(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let panel = &app.clients;
    let infos = panel.merged();
    if infos.is_empty() {
        let dir = panel
            .vault_dir
            .as_ref()
            .map(|d| d.display().to_string())
            .unwrap_or_else(|| "~/.config/degenbox/vault".into());
        let lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  No wallets in the vault yet ({dir})."),
                app.theme.muted(),
            )),
            Line::from(""),
            Line::from(vec![
                Span::raw("  "),
                Span::styled("[a]", app.theme.accent().add_modifier(Modifier::BOLD)),
                Span::styled(" generate a Solana wallet    ", app.theme.neutral()),
                Span::styled("[i]", app.theme.accent().add_modifier(Modifier::BOLD)),
                Span::styled(" import a private key (sol / hl)", app.theme.neutral()),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Shared with the DegenBox Signer desktop app — wallets added there appear here.",
                app.theme.muted(),
            )),
        ];
        let p = Paragraph::new(lines).block(widgets::panel("Clients", app.theme));
        frame.render_widget(p, area);
        return;
    }

    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("CHAIN"),
        Cell::from("LABEL"),
        Cell::from("ADDRESS"),
        Cell::from("ROLE"),
        Cell::from("RUNTIME"),
        Cell::from("GATEWAY"),
    ])
    .style(app.theme.muted().add_modifier(Modifier::BOLD));

    let mut rows: Vec<Row> = Vec::with_capacity(infos.len());
    for (i, info) in infos.iter().enumerate() {
        // Server-only rows (`gw-…`) have no vault entry; local rows map
        // 1:1 onto panel.entries by index order (merge preserves it).
        let entry = panel.entries.get(i);
        let selected = i == panel.sel && entry.is_some();
        let role = if info.primary {
            "primary"
        } else if entry.is_some() {
            "secondary"
        } else {
            "remote"
        };
        let runtime = match entry {
            Some(e) => {
                if e.paused {
                    format!("{} · PAUSED", runtime_state_for(app, e))
                } else {
                    runtime_state_for(app, e)
                }
            }
            None => "remote".into(),
        };
        let gw_col = match (&info.gateway, &info.drift) {
            (_, Some(d)) => d.clone(),
            (Some(g), None) => {
                let pos = g
                    .open_positions
                    .map(|p| format!("{p} pos"))
                    .unwrap_or_else(|| "ok".into());
                format!("registered · {pos}")
            }
            (None, None) => "—".into(),
        };
        let style = if entry.is_some_and(|e| e.paused) {
            app.theme.warn()
        } else if selected {
            app.theme.neutral().add_modifier(Modifier::BOLD)
        } else {
            app.theme.neutral()
        };
        rows.push(
            Row::new(vec![
                Cell::from(format!(
                    "{}{}",
                    if selected { "›" } else { " " },
                    if info.primary { "★" } else { " " }
                )),
                Cell::from(info.chain.clone()),
                Cell::from(info.label.clone().unwrap_or_else(|| "—".into())),
                Cell::from(short_addr(&info.address)),
                Cell::from(role),
                Cell::from(runtime),
                Cell::from(gw_col),
            ])
            .style(style),
        );
    }

    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(5),
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Length(8),
            Constraint::Length(20),
            Constraint::Min(16),
        ],
    )
    .header(header)
    .block(widgets::panel("Clients — wallet fleet (vault)", app.theme));
    frame.render_widget(table, area);
}

fn render_status(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let panel = &app.clients;
    let gw = panel
        .gateway
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or(GatewayFetch::NotStarted);
    let gw_line = match gw {
        GatewayFetch::NotStarted => {
            "gateway: not fetched — press [g] to merge the server registry".to_string()
        }
        GatewayFetch::Pending => "gateway: fetching…".to_string(),
        GatewayFetch::Done(Some(rows)) => format!("gateway: {} row(s) merged", rows.len()),
        GatewayFetch::Done(None) => {
            "gateway: clients endpoint unavailable — local-only view".to_string()
        }
        GatewayFetch::Failed(e) => format!("gateway: {e}"),
    };
    let mut lines = vec![Line::from(vec![
        Span::raw("  "),
        Span::styled(gw_line, app.theme.muted()),
    ])];
    if let Some(n) = &panel.notice {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(n.clone(), app.theme.warn()),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  ★ primary owns legacy work; secondary HL wallets execute wallet-scoped work \
             on multi-client gateways",
            app.theme.muted(),
        )));
    }
    let p = Paragraph::new(lines).block(widgets::panel("Fleet status", app.theme));
    frame.render_widget(p, area);
}

fn render_hints(app: &App, frame: &mut Frame<'_>, area: Rect) {
    let p = Paragraph::new(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "[↑/↓] select  [a] add sol  [i] import  [p] pause  [P] set primary  [e] label  \
             [x] remove  [g] gateway refresh",
            app.theme.muted(),
        ),
    ]));
    frame.render_widget(p, area);
}

pub fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> AppOutcome {
    match (code, mods) {
        (KeyCode::Up, _) => {
            app.clients.sel = app.clients.sel.saturating_sub(1);
            AppOutcome::Continue
        }
        (KeyCode::Down, _) => {
            if app.clients.sel + 1 < app.clients.entries.len() {
                app.clients.sel += 1;
            }
            AppOutcome::Continue
        }
        (KeyCode::Char('g'), _) | (KeyCode::Char('r'), _) => {
            app.clients.reload();
            app.clients.refresh_gateway();
            AppOutcome::Continue
        }
        (KeyCode::Char('a'), _) => {
            app.modal = Some(Modal::ClientAdd {
                step: 0,
                label: String::new(),
                input: String::new(),
                error: None,
            });
            AppOutcome::Continue
        }
        (KeyCode::Char('i'), _) => {
            app.modal = Some(Modal::ClientImport {
                step: 0,
                chain: WalletChain::Sol,
                secret: String::new(),
                label: String::new(),
                input: String::new(),
                error: None,
            });
            AppOutcome::Continue
        }
        (KeyCode::Char('e'), _) => {
            if let Some(entry) = app.clients.selected() {
                app.modal = Some(Modal::ClientLabel {
                    id: entry.id.clone(),
                    input: entry.label.clone().unwrap_or_default(),
                    error: None,
                });
            }
            AppOutcome::Continue
        }
        (KeyCode::Char('x'), _) => {
            if let Some(entry) = app.clients.selected() {
                app.modal = Some(Modal::Confirm {
                    title: "Remove client".into(),
                    body: format!(
                        "Remove {} ({}) from the vault? The encrypted keystore is kept \
                         as .removed.bak — this never destroys key material.",
                        short_addr(&entry.address),
                        entry.chain.as_str()
                    ),
                    on_yes: ConfirmAction::RemoveClient {
                        id: entry.id.clone(),
                    },
                });
            }
            AppOutcome::Continue
        }
        (KeyCode::Char('p'), KeyModifiers::NONE) => {
            toggle_pause_selected(app);
            AppOutcome::Continue
        }
        (KeyCode::Char('P'), _) | (KeyCode::Char('p'), KeyModifiers::SHIFT) => {
            set_primary_selected(app);
            AppOutcome::Continue
        }
        _ => AppOutcome::Continue,
    }
}

/// Per-client pause: persist in the vault, mirror into the live bot
/// pause gate (HL) / stop-start hint (Sol), push to the gateway
/// best-effort. Same semantics as the app's `client_pause`.
fn toggle_pause_selected(app: &mut App) {
    let Some(entry) = app.clients.selected().cloned() else {
        return;
    };
    let paused = !entry.paused;
    let res = crate::clients::open_vault()
        .and_then(|v| v.ok_or_else(|| "no vault on this device".into()))
        .and_then(|mut v| v.set_paused(&entry.id, paused).map_err(|e| e.to_string()));
    if let Err(e) = res {
        app.clients.notice = Some(format!("pause failed: {e}"));
        return;
    }
    // Mirror into the live HL pause gate (the daemon reads it inline).
    for bot in app.bots.iter_mut() {
        if bot.vault_id.as_deref() == Some(entry.id.as_str()) {
            if let Ok(mut p) = bot.pause.lock() {
                *p = paused;
            }
        }
    }
    // Sol primary: the engine has no inline gate — stop it on pause;
    // resume requires the keypair (unlock), so direct the operator.
    if entry.chain == WalletChain::Sol
        && Some(&entry.id) == app.clients.primary_sol.as_ref()
        && app.sol.is_unlocked()
    {
        if paused {
            app.sol.runtime.stop();
            app.clients.notice = Some(
                "Sol engine stopped (client paused). Resume + [u]nlock on the Solana tab to restart."
                    .into(),
            );
        } else {
            app.clients.notice =
                Some("Resumed. Re-unlock on the Solana tab to restart the engine.".into());
        }
    } else {
        app.clients.notice = Some(format!(
            "{} {} (persisted; gateway notified best-effort)",
            short_addr(&entry.address),
            if paused { "PAUSED" } else { "resumed" }
        ));
    }
    app.clients.reload();
    // Best-effort server sync off the UI thread.
    tokio::spawn(crate::clients::push_pause_best_effort(
        entry.address.clone(),
        paused,
    ));
}

/// Designate the selected wallet as its chain's primary. Persisted in
/// the vault; the executor swap applies on the next unlock/restart —
/// the TUI deliberately does NOT hot-swap a live HL daemon (two pollers
/// racing one user-scoped claim queue is a money-path hazard; the app
/// has CAS+generation machinery for this, the TUI keeps it simple).
fn set_primary_selected(app: &mut App) {
    let Some(entry) = app.clients.selected().cloned() else {
        return;
    };
    let res = crate::clients::open_vault()
        .and_then(|v| v.ok_or_else(|| "no vault on this device".into()))
        .and_then(|mut v| v.set_primary(&entry.id).map_err(|e| e.to_string()));
    match res {
        Ok(()) => {
            app.clients.reload();
            let any_running = app.bots.iter().any(|b| b.is_running());
            app.modal = Some(Modal::Message {
                title: "Primary changed".into(),
                body: format!(
                    "{} is now the {} primary.{}",
                    short_addr(&entry.address),
                    entry.chain.as_str(),
                    if any_running || app.sol.is_unlocked() {
                        " Restart the signer (quit + relaunch, or re-unlock) to swap the \
                         live executor — roles are applied at unlock."
                    } else {
                        " It becomes the executor on the next unlock."
                    }
                ),
            });
        }
        Err(e) => app.clients.notice = Some(format!("set-primary failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tui::app::{App, RuntimeState, Tab};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn dump(app: &App) -> String {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(app, f, f.area())).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_empty_state_with_add_hints() {
        let app = App::for_test(Config::default(), RuntimeState::new());
        let d = dump(&app);
        assert!(d.contains("Clients"));
        assert!(d.contains("[a] generate"));
        assert!(d.contains("desktop app"));
    }

    #[test]
    fn renders_fleet_table_with_roles_and_pause() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.clients.entries = vec![
            WalletEntry {
                id: "id-sol".into(),
                chain: WalletChain::Sol,
                address: "So1AAAAAAAAAAAAAAAAAAAAA".into(),
                label: Some("main".into()),
                created_at: chrono::Utc::now(),
                file: "sol-x.json".into(),
                paused: false,
            },
            WalletEntry {
                id: "id-hl".into(),
                chain: WalletChain::Hl,
                address: "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd".into(),
                label: None,
                created_at: chrono::Utc::now(),
                file: "hl-x.json".into(),
                paused: true,
            },
        ];
        app.clients.primary_sol = Some("id-sol".into());
        app.clients.primary_hl = Some("id-hl".into());
        let d = dump(&app);
        assert!(d.contains("★"), "primary star rendered");
        assert!(d.contains("main"));
        assert!(d.contains("PAUSED"));
        assert!(d.contains("primary"));
        assert!(d.contains("[g] gateway refresh"));
    }

    #[test]
    fn add_and_import_keys_open_modals() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.tab = Tab::Clients;
        app.on_key(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(app.modal, Some(Modal::ClientAdd { step: 0, .. })));
        app.modal = None;
        app.on_key(KeyCode::Char('i'), KeyModifiers::NONE);
        assert!(matches!(
            app.modal,
            Some(Modal::ClientImport { step: 0, .. })
        ));
        // Chain choice advances the import wizard.
        app.on_key(KeyCode::Char('h'), KeyModifiers::NONE);
        match &app.modal {
            Some(Modal::ClientImport { step, chain, .. }) => {
                assert_eq!(*step, 1);
                assert_eq!(*chain, WalletChain::Hl);
            }
            other => panic!("unexpected modal {other:?}"),
        }
    }

    #[test]
    fn selection_clamps_to_fleet_size() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.tab = Tab::Clients;
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(app.clients.sel, 0, "no entries — selection pinned at 0");
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.clients.sel, 0);
    }
}
