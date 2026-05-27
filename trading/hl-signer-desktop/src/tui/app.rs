//! `App` — TUI state machine.
//!
//! Note: fields like `RuntimeState::error`, parts of `OrderRow`, and
//! `Modal::Confirm` are wired up to surface here from the background
//! daemon-bridge task — that task lands in a follow-up sprint, so
//! several fields/variants read as dead in this build. Keep the data
//! model complete so the follow-up is purely additive.
#![allow(dead_code)]
//
// The original module docs follow below.
//!
//! Owns:
//! - the active tab
//! - the runtime view of daemon/connection state (a bunch of
//!   `Arc<Mutex<...>>` that a background task can update)
//! - a snapshot of recent signed orders (last 50)
//! - the in-flight modal (unlock prompt, reveal-phrase, confirm)
//! - the editable settings draft
//!
//! Render functions live in `screens::*` and are pure: they take
//! `&App` and produce ratatui widgets. Side-effecting handlers
//! (unlock keystore, save settings) live here.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::Frame;

use crate::config::{self, Config};
use crate::keystore;

use super::log_capture::LogBuffer;
use super::screens;
use super::theme::Theme;
use super::widgets;
use super::TAB_ORDER;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Status,
    Wallet,
    Settings,
    Logs,
}

impl Tab {
    pub fn label(&self) -> &'static str {
        match self {
            Tab::Status => "Status",
            Tab::Wallet => "Wallet",
            Tab::Settings => "Settings",
            Tab::Logs => "Logs",
        }
    }

    pub fn index(&self) -> usize {
        TAB_ORDER.iter().position(|t| t == self).unwrap_or(0)
    }
}

/// Connection lifecycle the status pill projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    Offline,
    Connecting,
    Ready,
    Paused,
    Error,
}

impl ConnState {
    pub fn label(&self) -> &'static str {
        match self {
            ConnState::Offline => "OFFLINE",
            ConnState::Connecting => "CONNECTING",
            ConnState::Ready => "READY",
            ConnState::Paused => "PAUSED",
            ConnState::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct OrderRow {
    pub at: chrono::DateTime<chrono::Utc>,
    pub coin: String,
    pub side: String,
    pub size_usd: String,
    pub status: String,
    pub fill_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct QueueSnapshot {
    pub pending: usize,
    pub last_drained_at: Option<chrono::DateTime<chrono::Utc>>,
    pub next_preview: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeState {
    pub conn: Option<ConnState>,
    pub user_id: Option<String>,
    pub orders: VecDeque<OrderRow>,
    pub queue: QueueSnapshot,
    pub error: Option<String>,
    pub update_available: Option<String>,
}

impl RuntimeState {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.conn = Some(ConnState::Offline);
        s
    }

    pub fn push_order(&mut self, row: OrderRow) {
        if self.orders.len() >= 50 {
            self.orders.pop_front();
        }
        self.orders.push_back(row);
    }
}

#[derive(Debug, Clone)]
pub enum Modal {
    Unlock {
        input: String,
        error: Option<String>,
    },
    RevealPhrase {
        input: String,
        revealed: Option<String>,
        error: Option<String>,
    },
    Confirm {
        title: String,
        body: String,
        on_yes: ConfirmAction,
    },
    Message {
        title: String,
        body: String,
    },
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    QuitDaemon,
    DeleteKeystore,
}

#[derive(Debug, Clone)]
pub enum AppOutcome {
    Continue,
    Quit,
}

/// Editable draft used by the Settings tab. The user types into this
/// and `save` persists it into `Config`.
#[derive(Debug, Clone)]
pub struct SettingsDraft {
    pub poll_secs: String,
    pub log_level: String,
    pub paper_mode: bool,
    pub auto_update: bool,
    pub max_clients: String,
    pub focus_row: usize,
}

impl SettingsDraft {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            poll_secs: "3".to_string(),
            log_level: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            paper_mode: false,
            auto_update: true,
            max_clients: "1".into(),
            focus_row: 0,
        }
        .with_server(cfg)
    }

    fn with_server(mut self, _cfg: &Config) -> Self {
        // The server URL itself is rendered read-only; the editable
        // rows are above. Future revs may make it editable too.
        self
    }
}

pub struct App {
    pub theme: Theme,
    pub tab: Tab,
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub ks_path: PathBuf,
    pub agent_address: Option<String>,
    pub host_id: Option<String>,
    pub started_at: Instant,
    pub runtime: Arc<Mutex<RuntimeState>>,
    pub modal: Option<Modal>,
    pub log_buf: LogBuffer,
    pub settings: SettingsDraft,
    pub footer_hint: String,
    /// Click hit-boxes for the tabs, set during render. `(start, end)`
    /// column on the tab-strip row.
    pub tab_hits: Vec<(u16, u16)>,
    /// y-row where the tab strip lives — cached during render so click
    /// handler can route the mouse correctly.
    pub tab_row: u16,
}

impl App {
    pub fn bootstrap(ks_path: PathBuf, cfg_path: PathBuf, log_buf: LogBuffer) -> Result<Self> {
        let cfg = config::load(&cfg_path).unwrap_or_default();
        let agent_address = keystore::peek_address(&ks_path).ok();
        let settings = SettingsDraft::from_config(&cfg);
        Ok(Self {
            theme: Theme::from_env(),
            tab: Tab::Status,
            host_id: cfg.host_id.clone(),
            cfg,
            cfg_path,
            ks_path,
            agent_address,
            started_at: Instant::now(),
            runtime: Arc::new(Mutex::new(RuntimeState::new())),
            modal: None,
            log_buf,
            settings,
            footer_hint: default_footer().to_string(),
            tab_hits: Vec::new(),
            tab_row: 0,
        })
    }

    /// Build an `App` from explicit pieces — used by snapshot tests.
    #[cfg(test)]
    pub fn for_test(cfg: Config, runtime: RuntimeState) -> Self {
        let log_buf = LogBuffer::new();
        let settings = SettingsDraft::from_config(&cfg);
        Self {
            theme: Theme::plain(),
            tab: Tab::Status,
            host_id: cfg.host_id.clone(),
            cfg,
            cfg_path: PathBuf::from("/tmp/cfg.json"),
            ks_path: PathBuf::from("/tmp/ks.json"),
            agent_address: Some("0x0000000000000000000000000000000000000001".to_string()),
            started_at: Instant::now(),
            runtime: Arc::new(Mutex::new(runtime)),
            modal: None,
            log_buf,
            settings,
            footer_hint: default_footer().to_string(),
            tab_hits: Vec::new(),
            tab_row: 0,
        }
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    pub fn on_tick(&mut self) {
        // Hook for periodic state refreshes. The daemon-driven
        // updates flow through `runtime` via background tasks; the
        // tick keeps the time-relative widgets (uptime, "last drained
        // 4s ago") moving even when nothing else is happening.
    }

    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> AppOutcome {
        // Modal traps every keystroke until dismissed.
        if self.modal.is_some() {
            return self.on_key_modal(code, mods);
        }
        match (code, mods) {
            (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => AppOutcome::Quit,
            (KeyCode::Tab, KeyModifiers::NONE) => {
                self.tab = next_tab(self.tab, 1);
                AppOutcome::Continue
            }
            (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => {
                self.tab = next_tab(self.tab, -1);
                AppOutcome::Continue
            }
            (KeyCode::Char('1'), _) => {
                self.tab = Tab::Status;
                AppOutcome::Continue
            }
            (KeyCode::Char('2'), _) => {
                self.tab = Tab::Wallet;
                AppOutcome::Continue
            }
            (KeyCode::Char('3'), _) => {
                self.tab = Tab::Settings;
                AppOutcome::Continue
            }
            (KeyCode::Char('4'), _) => {
                self.tab = Tab::Logs;
                AppOutcome::Continue
            }
            (KeyCode::Char('p'), _) if self.tab == Tab::Status => {
                if let Ok(mut rt) = self.runtime.lock() {
                    rt.conn = Some(match rt.conn {
                        Some(ConnState::Paused) => ConnState::Ready,
                        _ => ConnState::Paused,
                    });
                }
                AppOutcome::Continue
            }
            _ => screens::handle_key(self, code, mods),
        }
    }

    fn on_key_modal(&mut self, code: KeyCode, _mods: KeyModifiers) -> AppOutcome {
        let modal = match self.modal.take() {
            Some(m) => m,
            None => return AppOutcome::Continue,
        };
        match modal {
            Modal::Unlock { mut input, .. } => {
                match code {
                    KeyCode::Esc => AppOutcome::Continue,
                    KeyCode::Enter => {
                        match keystore::decrypt(&self.ks_path, input.as_bytes()) {
                            Ok((_secret, addr)) => {
                                self.agent_address = Some(addr);
                                if let Ok(mut rt) = self.runtime.lock() {
                                    rt.conn = Some(ConnState::Connecting);
                                }
                                self.modal = Some(Modal::Message {
                                title: "Unlocked".into(),
                                body: "Keystore unlocked. Use the Settings tab to start the daemon.".into(),
                            });
                            }
                            Err(e) => {
                                self.modal = Some(Modal::Unlock {
                                    input: String::new(),
                                    error: Some(e.to_string()),
                                });
                            }
                        }
                        AppOutcome::Continue
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        self.modal = Some(Modal::Unlock { input, error: None });
                        AppOutcome::Continue
                    }
                    KeyCode::Char(c) => {
                        input.push(c);
                        self.modal = Some(Modal::Unlock { input, error: None });
                        AppOutcome::Continue
                    }
                    _ => {
                        self.modal = Some(Modal::Unlock { input, error: None });
                        AppOutcome::Continue
                    }
                }
            }
            Modal::RevealPhrase { mut input, .. } => match code {
                KeyCode::Esc => AppOutcome::Continue,
                KeyCode::Enter => match keystore::decrypt(&self.ks_path, input.as_bytes()) {
                    Ok((secret, _addr)) => {
                        self.modal = Some(Modal::RevealPhrase {
                            input: String::new(),
                            revealed: Some(secret),
                            error: None,
                        });
                        AppOutcome::Continue
                    }
                    Err(e) => {
                        self.modal = Some(Modal::RevealPhrase {
                            input: String::new(),
                            revealed: None,
                            error: Some(e.to_string()),
                        });
                        AppOutcome::Continue
                    }
                },
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::RevealPhrase {
                        input,
                        revealed: None,
                        error: None,
                    });
                    AppOutcome::Continue
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.modal = Some(Modal::RevealPhrase {
                        input,
                        revealed: None,
                        error: None,
                    });
                    AppOutcome::Continue
                }
                _ => {
                    self.modal = Some(Modal::RevealPhrase {
                        input,
                        revealed: None,
                        error: None,
                    });
                    AppOutcome::Continue
                }
            },
            Modal::Confirm {
                title,
                body,
                on_yes,
            } => match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => match on_yes {
                    ConfirmAction::QuitDaemon => AppOutcome::Quit,
                    ConfirmAction::DeleteKeystore => {
                        let _ = std::fs::remove_file(&self.ks_path);
                        self.agent_address = None;
                        AppOutcome::Continue
                    }
                },
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => AppOutcome::Continue,
                _ => {
                    self.modal = Some(Modal::Confirm {
                        title,
                        body,
                        on_yes,
                    });
                    AppOutcome::Continue
                }
            },
            Modal::Message { .. } => match code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => AppOutcome::Continue,
                _ => AppOutcome::Continue,
            },
        }
    }

    pub fn on_mouse(&mut self, col: u16, row: u16) {
        if row != self.tab_row {
            return;
        }
        for (i, (start, end)) in self.tab_hits.iter().enumerate() {
            if col >= *start && col <= *end {
                if let Some(tab) = TAB_ORDER.get(i).copied() {
                    self.tab = tab;
                }
                return;
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame<'_>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // header
                Constraint::Length(1), // tab strip
                Constraint::Min(0),    // body
                Constraint::Length(1), // footer
            ])
            .split(frame.area());
        self.render_header(frame, chunks[0]);
        self.render_tabs(frame, chunks[1]);
        self.render_body(frame, chunks[2]);
        widgets::footer(frame, chunks[3], &self.footer_hint, self.theme);
        if let Some(modal) = self.modal.clone() {
            widgets::modal(frame, &modal, self.theme);
        }
    }

    fn render_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let conn = self
            .runtime
            .lock()
            .ok()
            .and_then(|r| r.conn)
            .unwrap_or(ConnState::Offline);
        let update = self
            .runtime
            .lock()
            .ok()
            .and_then(|r| r.update_available.clone());
        widgets::header(
            frame,
            area,
            widgets::HeaderProps {
                conn,
                uptime: self.uptime(),
                version: env!("CARGO_PKG_VERSION"),
                update_available: update,
            },
            self.theme,
        );
    }

    fn render_tabs(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let (hits, row) = widgets::tabs(frame, area, self.tab, self.theme);
        self.tab_hits = hits;
        self.tab_row = row;
    }

    fn render_body(&self, frame: &mut Frame<'_>, area: Rect) {
        match self.tab {
            Tab::Status => screens::status::render(self, frame, area),
            Tab::Wallet => screens::wallet::render(self, frame, area),
            Tab::Settings => screens::settings::render(self, frame, area),
            Tab::Logs => screens::logs::render(self, frame, area),
        }
    }
}

pub fn next_tab(current: Tab, dir: i32) -> Tab {
    let i = current.index() as i32;
    let len = TAB_ORDER.len() as i32;
    let next = ((i + dir).rem_euclid(len)) as usize;
    TAB_ORDER[next]
}

fn default_footer() -> &'static str {
    "[Tab] switch  [q] quit  [p] pause  [u] unlock  [1-4] jump"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_cycles_forward_and_backward() {
        assert_eq!(next_tab(Tab::Status, 1), Tab::Wallet);
        assert_eq!(next_tab(Tab::Logs, 1), Tab::Status);
        assert_eq!(next_tab(Tab::Status, -1), Tab::Logs);
    }

    #[test]
    fn pause_toggles_conn_state() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        // Force the runtime to Ready first, otherwise the toggle is
        // ambiguous (any non-Paused → Paused).
        {
            let mut rt = app.runtime.lock().unwrap();
            rt.conn = Some(ConnState::Ready);
        }
        app.on_key(KeyCode::Char('p'), KeyModifiers::NONE);
        assert_eq!(app.runtime.lock().unwrap().conn.unwrap(), ConnState::Paused);
        app.on_key(KeyCode::Char('p'), KeyModifiers::NONE);
        assert_eq!(app.runtime.lock().unwrap().conn.unwrap(), ConnState::Ready);
    }

    #[test]
    fn modal_traps_input_until_dismissed() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.modal = Some(Modal::Message {
            title: "x".into(),
            body: "y".into(),
        });
        let before = app.tab;
        app.on_key(KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.tab, before, "tab change ignored while modal up");
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.modal.is_none(), "modal dismissed on Enter");
    }

    #[test]
    fn order_ring_caps_at_fifty() {
        let mut rt = RuntimeState::new();
        for i in 0..60 {
            rt.push_order(OrderRow {
                at: chrono::Utc::now(),
                coin: "BTC".into(),
                side: "buy".into(),
                size_usd: format!("{i}"),
                status: "ok".into(),
                fill_ms: None,
            });
        }
        assert_eq!(rt.orders.len(), 50);
        assert_eq!(rt.orders.front().unwrap().size_usd, "10");
    }
}
