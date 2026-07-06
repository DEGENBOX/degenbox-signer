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
use degenbox_signer_core::hl::runtime::{ConnState as CoreConnState, HlRuntime, SharedHlRuntime};
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
    Solana,
    Clients,
    Settings,
    Logs,
}

impl Tab {
    pub fn label(&self) -> &'static str {
        match self {
            Tab::Status => "Status",
            Tab::Wallet => "Wallet",
            Tab::Solana => "Solana",
            Tab::Clients => "Clients",
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
    /// Realised PnL string on a close/reduce fill, when the signer
    /// resolved it. `None` for opens / resting / unresolved.
    pub pnl: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct QueueSnapshot {
    pub pending: usize,
    pub last_drained_at: Option<chrono::DateTime<chrono::Utc>>,
    pub next_preview: Option<String>,
}

/// One open HL perp position, as rendered in the Wallet/Status balance
/// panels. Sourced from the MASTER account's `clearinghouseState`.
#[derive(Debug, Clone)]
pub struct PositionRow {
    pub coin: String,
    /// Signed size — positive long, negative short.
    pub szi: String,
    /// Side label derived from the sign of `szi`.
    pub side: String,
}

/// Per-client account balance snapshot, fetched off the MASTER account
/// (never the agent — the agent always reads $0). Refreshed by a
/// background balance task; `None` until the first successful fetch.
#[derive(Debug, Clone, Default)]
pub struct BalanceSnapshot {
    /// `marginSummary.accountValue` in USD.
    pub account_value_usd: Option<String>,
    /// Free / withdrawable USD (`withdrawable`), when present.
    pub withdrawable_usd: Option<String>,
    pub positions: Vec<PositionRow>,
    pub fetched_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Last balance-fetch error (network / parse), surfaced in the UI
    /// instead of a misleading $0.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeState {
    pub conn: Option<ConnState>,
    pub user_id: Option<String>,
    /// Discord handle resolved at register, for a friendlier label.
    pub discord_handle: Option<String>,
    pub orders: VecDeque<OrderRow>,
    pub queue: QueueSnapshot,
    pub error: Option<String>,
    pub update_available: Option<String>,
    /// Wall-clock of the last successful poll round-trip — drives the
    /// "last poll Ns ago" heartbeat in the dashboard.
    pub last_poll_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Live balance + open positions for the MASTER account.
    pub balance: BalanceSnapshot,
    /// `true` when this client is dry-run (paper) — set from config so
    /// the UI can badge it and the daemon refuses to submit.
    pub paper_mode: bool,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self {
            conn: Some(ConnState::Offline),
            ..Self::default()
        }
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
    /// Per-trade 2FA: the gateway challenged a `post_result` and the
    /// core daemon parked the prompt — the operator types the 6-digit
    /// code here (the daemon waits a bounded 90 s, then fails the ack).
    Totp {
        bot_idx: usize,
        challenge_id: String,
        expires_at: String,
        input: String,
    },
    /// Unlock the Solana keystore (arms the `:5829` slot + starts the
    /// sell/copy execution runtime).
    SolUnlock {
        input: String,
        error: Option<String>,
    },
    /// Set / change the mandatory copy-session budget (SOL). Empty
    /// input disarms copy buys.
    SolBudget {
        input: String,
        error: Option<String>,
    },
    /// Edit a vault client's label (Clients tab).
    ClientLabel {
        id: String,
        input: String,
        error: Option<String>,
    },
    /// Generate a fresh Solana wallet into the vault: step 0 = label
    /// (optional), step 1 = master password.
    ClientAdd {
        step: u8,
        label: String,
        input: String,
        error: Option<String>,
    },
    /// Import a pasted private key into the vault: step 0 = chain
    /// ([s]ol / [h]l), step 1 = secret (hidden), step 2 = label,
    /// step 3 = master password.
    ClientImport {
        step: u8,
        chain: degenbox_signer_core::WalletChain,
        secret: String,
        label: String,
        input: String,
        error: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    QuitDaemon,
    DeleteKeystore,
    /// Remove a vault client (keystore kept as `.removed.bak`).
    RemoveClient {
        id: String,
    },
}

#[derive(Debug, Clone)]
pub enum AppOutcome {
    Continue,
    Quit,
    RunWizard,
}

/// Editable draft used by the Settings tab. The user types into this
/// and `save` persists it into `Config`.
#[derive(Debug, Clone)]
pub struct SettingsDraft {
    pub poll_secs: String,
    pub log_level: String,
    pub paper_mode: bool,
    pub auto_update: bool,
    pub focus_row: usize,
    pub is_editing: bool,
}

impl SettingsDraft {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            // Read the REAL per-bot cadence instead of a hardcoded "3".
            poll_secs: cfg.poll_secs.to_string(),
            log_level: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
            paper_mode: false,
            auto_update: true,
            focus_row: 0,
            is_editing: false,
        }
    }
}

/// Which role a vault wallet plays in the serialized multi-client
/// topology (the gateway work queues are user-scoped today — exactly
/// one executor per chain; see `clients.rs` module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultRole {
    Primary,
    Standby,
}

pub struct BotContext {
    pub name: String,
    pub dir: PathBuf,
    pub cfg: Config,
    pub ks_path: PathBuf,
    pub agent_address: Option<String>,
    pub runtime: Arc<Mutex<RuntimeState>>,
    /// The core daemon's live telemetry (conn state, balances, queue
    /// depth, parked TOTP prompt). Mirrored into `runtime` on tick.
    pub hl_runtime: SharedHlRuntime,
    pub daemon_handle: Option<tokio::task::JoinHandle<Result<()>>>,
    /// Shared pause flag handed to the daemon. Flipping it actually
    /// stops the daemon claiming/signing/submitting — not just a label.
    pub pause: crate::daemon::SharedPause,
    /// Last unlock error for THIS bot (wrong passphrase, corrupt
    /// keystore, …), surfaced so the operator knows which client failed.
    pub unlock_error: Option<String>,
    /// Vault wallet id when this bot comes from the shared multi-wallet
    /// vault (`None` = legacy `bots/` hub client).
    pub vault_id: Option<String>,
    /// Vault topology role. `Standby` bots never spawn the full
    /// poll/sign daemon — they run the heartbeat+balance loop only.
    pub vault_role: Option<VaultRole>,
    /// Per-wallet executed-marker ledger (vault wallets only) — two HL
    /// daemons must never share one idempotency ledger.
    pub executed_override: Option<PathBuf>,
}

impl BotContext {
    /// Whether this bot's keystore has been unlocked + its daemon spawned.
    pub fn is_running(&self) -> bool {
        self.daemon_handle
            .as_ref()
            .is_some_and(|h| !h.is_finished())
    }

    /// True if the operator paused this client.
    pub fn is_paused(&self) -> bool {
        self.pause.lock().map(|g| *g).unwrap_or(false)
    }
}

/// Build a `BotContext` for one vault HL wallet. The per-wallet
/// pairing config wins; the legacy global `hl-config.json` backs the
/// primary (migration keeps them synced); standbys without a per-wallet
/// pairing run unpaired (heartbeat skipped until paired).
pub fn vault_bot_context(
    vault: &degenbox_signer_core::Vault,
    entry: &degenbox_signer_core::WalletEntry,
    is_primary: bool,
) -> BotContext {
    let cfg = crate::clients::hl_config_for(vault, entry, is_primary);
    let name = entry.label.clone().unwrap_or_else(|| {
        let a = &entry.address;
        if a.len() > 12 {
            format!("{}…{}", &a[..6], &a[a.len() - 4..])
        } else {
            a.clone()
        }
    });
    BotContext {
        name,
        dir: vault.dir().to_path_buf(),
        cfg,
        ks_path: vault.keystore_path(entry),
        agent_address: Some(entry.address.clone()),
        runtime: Arc::new(Mutex::new(RuntimeState::new())),
        hl_runtime: Arc::new(HlRuntime::default()),
        daemon_handle: None,
        pause: Arc::new(Mutex::new(entry.paused)),
        unlock_error: None,
        vault_id: Some(entry.id.clone()),
        vault_role: Some(if is_primary {
            VaultRole::Primary
        } else {
            VaultRole::Standby
        }),
        executed_override: Some(crate::clients::hl_executed_path_seeded(vault, entry)),
    }
}

/// Map the core daemon's connection state onto the TUI's pill enum.
fn conn_from_core(c: CoreConnState) -> ConnState {
    match c {
        CoreConnState::Offline => ConnState::Offline,
        CoreConnState::Connecting => ConnState::Connecting,
        CoreConnState::Ready => ConnState::Ready,
        CoreConnState::Paused => ConnState::Paused,
        CoreConnState::Error => ConnState::Error,
    }
}

/// Mirror the core daemon's shared telemetry into the TUI's render
/// model. The orders ring + queue preview are written directly by the
/// daemon's event bridge; everything else (conn, identity, balances,
/// queue depth, last error) is owned by the core runtime and copied
/// here once per tick.
fn sync_runtime_from_core(core: &HlRuntime, tui_rt: &Arc<Mutex<RuntimeState>>) {
    let Ok(mut rt) = tui_rt.lock() else { return };
    if let Ok(g) = core.conn.lock() {
        if let Some(c) = *g {
            rt.conn = Some(conn_from_core(c));
        }
    }
    if let Ok(g) = core.user_id.lock() {
        if g.is_some() {
            rt.user_id = g.clone();
        }
    }
    if let Ok(g) = core.discord_handle.lock() {
        if g.is_some() {
            rt.discord_handle = g.clone();
        }
    }
    if let Ok(g) = core.paper_mode.lock() {
        rt.paper_mode = *g;
    }
    if let Ok(g) = core.last_poll_at.lock() {
        rt.last_poll_at = *g;
    }
    if let Ok(g) = core.queue_pending.lock() {
        rt.queue.pending = *g;
    }
    if let Ok(g) = core.error.lock() {
        rt.error = g.clone();
    }
    if let Ok(g) = core.balance.lock() {
        rt.balance = BalanceSnapshot {
            account_value_usd: g.account_value_usd.clone(),
            withdrawable_usd: g.withdrawable_usd.clone(),
            positions: g
                .positions
                .iter()
                .map(|p| PositionRow {
                    coin: p.coin.clone(),
                    szi: p.szi.clone(),
                    side: p.side.clone(),
                })
                .collect(),
            fetched_at: g.fetched_at,
            error: g.error.clone(),
        };
    }
}

pub struct App {
    pub theme: Theme,
    pub tab: Tab,
    pub bots: Vec<BotContext>,
    pub active_bot_idx: usize,
    pub started_at: Instant,
    pub modal: Option<Modal>,
    pub log_buf: LogBuffer,
    pub settings: SettingsDraft,
    pub footer_hint: String,
    pub tab_hits: Vec<(u16, u16)>,
    pub tab_row: u16,
    /// Solana execution panel state (keystore, runtime, :5829 daemon).
    pub sol: crate::sol::tui::SolPanel,
    /// Clients tab state (vault fleet + gateway merge).
    pub clients: screens::clients::ClientsPanel,
    /// Challenge id of a TOTP prompt the operator explicitly dismissed —
    /// suppresses re-raising the modal for the SAME challenge every tick
    /// (the daemon times out and fails the ack on its own).
    pub totp_dismissed: Option<String>,
}

impl App {
    pub fn bootstrap(log_buf: LogBuffer) -> Result<Self> {
        let discovered = config::discover_bots().unwrap_or_default();
        let mut bots = Vec::new();

        for (name, dir) in discovered {
            let cfg_path = dir.join("hl-config.json");
            let ks_path = dir.join("hl-keystore.json");
            let cfg = config::load(&cfg_path).unwrap_or_default();
            let agent_address = keystore::peek_address(&ks_path).ok();
            bots.push(BotContext {
                name,
                dir,
                cfg,
                ks_path,
                agent_address,
                runtime: Arc::new(Mutex::new(RuntimeState::new())),
                hl_runtime: Arc::new(HlRuntime::default()),
                daemon_handle: None,
                pause: Arc::new(Mutex::new(false)),
                unlock_error: None,
                vault_id: None,
                vault_role: None,
                executed_override: None,
            });
        }

        // Vault HL wallets (the multi-wallet vault shared with the
        // desktop app): the designated primary is the executor; every
        // other wallet stands by (heartbeat + balance only).
        if let Ok(Some(vault)) = crate::clients::open_vault() {
            use degenbox_signer_core::WalletChain;
            let primary_hl = vault.primary(WalletChain::Hl).map(|w| w.id.clone());
            for entry in vault.wallets() {
                if entry.chain != WalletChain::Hl {
                    continue;
                }
                let is_primary = Some(&entry.id) == primary_hl.as_ref();
                bots.push(vault_bot_context(&vault, entry, is_primary));
            }
        }

        // Seed Settings from the FIRST bot's real config (per-bot poll
        // cadence etc.), falling back to defaults when no bots exist yet so
        // the empty-state still lets the user reach "Create New Client".
        let settings =
            SettingsDraft::from_config(bots.first().map(|b| &b.cfg).unwrap_or(&Config::default()));
        let modal = if bots.is_empty() {
            None // Let them see the empty status screen, which directs them to settings
        } else {
            Some(Modal::Unlock {
                input: String::new(),
                error: None,
            })
        };

        Ok(Self {
            theme: Theme::from_env(),
            tab: Tab::Status,
            bots,
            active_bot_idx: 0,
            started_at: Instant::now(),
            modal,
            log_buf,
            settings,
            footer_hint: default_footer().to_string(),
            tab_hits: Vec::new(),
            tab_row: 0,
            sol: crate::sol::tui::SolPanel::bootstrap(),
            clients: screens::clients::ClientsPanel::bootstrap(),
            totp_dismissed: None,
        })
    }

    /// Build an `App` from explicit pieces — used by snapshot tests.
    #[cfg(test)]
    pub fn for_test(cfg: Config, runtime: RuntimeState) -> Self {
        let log_buf = LogBuffer::new();
        let settings = SettingsDraft::from_config(&cfg);
        let bots = vec![BotContext {
            name: "test-bot".into(),
            dir: PathBuf::from("/test"),
            cfg,
            ks_path: PathBuf::from("/test/hl-keystore.json"),
            agent_address: None,
            runtime: Arc::new(Mutex::new(runtime)),
            hl_runtime: Arc::new(HlRuntime::default()),
            daemon_handle: None,
            pause: Arc::new(Mutex::new(false)),
            unlock_error: None,
            vault_id: None,
            vault_role: None,
            executed_override: None,
        }];

        Self {
            theme: Theme::from_env(),
            tab: Tab::Status,
            bots,
            active_bot_idx: 0,
            started_at: Instant::now(),
            modal: None,
            log_buf,
            settings,
            footer_hint: String::new(),
            tab_hits: Vec::new(),
            tab_row: 0,
            sol: crate::sol::tui::SolPanel::for_test(),
            clients: screens::clients::ClientsPanel::for_test(),
            totp_dismissed: None,
        }
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    pub fn on_tick(&mut self) {
        let mut pending_totp: Option<(usize, String, String)> = None;
        for (i, bot) in self.bots.iter_mut().enumerate() {
            if let Some(h) = &bot.daemon_handle {
                if h.is_finished() {
                    // The daemon task exited (error / clean stop). Surface
                    // the join error if we can so the operator sees WHY.
                    bot.daemon_handle = None;
                    if let Ok(mut rt) = bot.runtime.lock() {
                        rt.conn = Some(ConnState::Error);
                        if rt.error.is_none() {
                            rt.error = Some("signer task stopped — check Logs".into());
                        }
                    }
                    continue;
                }
                // Daemon alive — mirror the core runtime's live telemetry
                // (conn / identity / balance / queue depth / error) into
                // the TUI model. The core daemon owns the truth.
                sync_runtime_from_core(&bot.hl_runtime, &bot.runtime);
                // A paused client always reads PAUSED instantly, even if
                // the daemon hasn't completed its next poll tick yet.
                if bot.is_paused() {
                    if let Ok(mut rt) = bot.runtime.lock() {
                        rt.conn = Some(ConnState::Paused);
                    }
                }
                // Per-trade TOTP: the core daemon parks the challenge in
                // the shared runtime and waits (bounded). Surface it as a
                // modal so the operator can answer in-screen.
                if pending_totp.is_none() {
                    if let Ok(g) = bot.hl_runtime.totp_prompt.lock() {
                        if let Some(p) = g.as_ref() {
                            pending_totp = Some((i, p.challenge_id.clone(), p.expires_at.clone()));
                        }
                    }
                }
            }
        }
        if self.modal.is_none() {
            if let Some((idx, challenge_id, expires_at)) = pending_totp {
                if self.totp_dismissed.as_deref() != Some(challenge_id.as_str()) {
                    self.modal = Some(Modal::Totp {
                        bot_idx: idx,
                        challenge_id,
                        expires_at,
                        input: String::new(),
                    });
                }
            }
        }
    }

    /// Try `password` against every NOT-yet-running bot. Each bot that
    /// decrypts is started (daemon spawned with its real pause/runtime
    /// handles); each that fails records a per-bot `unlock_error` so the
    /// UI can name it. Returns how many bots were freshly started.
    ///
    /// Per-bot passphrases: bots that don't match are left locked (with
    /// their error set) for the next Unlock prompt — never silently
    /// dropped.
    fn unlock_locked_bots(&mut self, password: &str) -> usize {
        let mut started = 0;
        for bot in self.bots.iter_mut() {
            if bot.is_running() {
                continue;
            }
            match keystore::decrypt(&bot.ks_path, password.as_bytes()) {
                Ok((secret_hex, addr)) => {
                    bot.agent_address = Some(addr.clone());
                    bot.unlock_error = None;
                    if let Ok(mut rt) = bot.runtime.lock() {
                        rt.conn = Some(ConnState::Connecting);
                        rt.error = None;
                        rt.paper_mode = self.settings.paper_mode;
                    }
                    // Vault SECONDARY wallets (app topology): probe the
                    // gateway for per-wallet claim scoping — multi-client
                    // gateway → full daemon with a STRICT wallet scope;
                    // older gateway / unpaired → heartbeat+balance standby
                    // (never claims the primary's instructions).
                    if bot.vault_role == Some(VaultRole::Standby) {
                        let entry = degenbox_signer_core::WalletEntry {
                            id: bot.vault_id.clone().unwrap_or_default(),
                            chain: degenbox_signer_core::WalletChain::Hl,
                            address: addr.clone(),
                            label: Some(bot.name.clone()),
                            created_at: chrono::Utc::now(),
                            file: String::new(),
                            paused: bot.is_paused(),
                        };
                        let executed = bot
                            .executed_override
                            .clone()
                            .unwrap_or_else(|| config::executed_path_in(&bot.dir));
                        bot.daemon_handle = Some(crate::clients::spawn_hl_secondary(
                            crate::clients::HlSecondaryArgs {
                                entry,
                                cfg: bot.cfg.clone(),
                                secret_hex,
                                hl_runtime: bot.hl_runtime.clone(),
                                tui_runtime: Some(bot.runtime.clone()),
                                pause: bot.pause.clone(),
                                executed_path: executed,
                                poll_secs: Some(bot.cfg.poll_secs.max(1)),
                                nats_url: bot.cfg.nats_url.clone(),
                            },
                        ));
                        started += 1;
                        continue;
                    }
                    // Vault wallets keep their persisted per-client
                    // pause across unlock; legacy bots reset to live.
                    if bot.vault_id.is_none() {
                        if let Ok(mut p) = bot.pause.lock() {
                            *p = false;
                        }
                    }
                    // Per-bot poll cadence: prefer the bot's own config,
                    // falling back to the Settings draft only when unset.
                    let poll = bot.cfg.poll_secs.max(1);
                    let opts = crate::daemon::DaemonOpts {
                        config: bot.cfg.clone(),
                        secret_hex,
                        agent_address: addr,
                        poll_interval: std::time::Duration::from_secs(poll),
                        // Persisted per-bot NATS push URL (usually unset —
                        // poll is the source of truth either way).
                        nats_url: bot.cfg.nats_url.clone(),
                        pause: Some(bot.pause.clone()),
                        runtime: Some(bot.runtime.clone()),
                        hl_runtime: bot.hl_runtime.clone(),
                        paper_mode: self.settings.paper_mode,
                        // Per-bot marker ledger lives in THIS bot's dir so two
                        // hub bots never share one in-memory ExecutedStore.
                        config_dir: Some(bot.dir.clone()),
                        // Vault wallets pin their own per-wallet ledger.
                        executed_path: bot.executed_override.clone(),
                        // Vault primaries claim wallet-scoped (refuse rows
                        // stamped for another master); legacy hub bots keep
                        // the unscoped claim — exact prior behaviour.
                        claim_scope: if bot.vault_role == Some(VaultRole::Primary) {
                            crate::clients::hl_primary_claim_scope(&bot.cfg)
                        } else {
                            crate::daemon::ClaimScope::Unscoped
                        },
                        // The TUI renders state itself + answers TOTP via a
                        // modal; no stderr banner / stdin prompt under raw mode.
                        banner: false,
                        stdin_totp: false,
                    };
                    let bot_name = bot.name.clone();
                    let runtime = bot.runtime.clone();
                    bot.daemon_handle = Some(tokio::spawn(async move {
                        let r = crate::daemon::run(opts).await;
                        if let Err(e) = &r {
                            tracing::error!(bot = %bot_name, "signer stopped: {e:#}");
                            if let Ok(mut rt) = runtime.lock() {
                                rt.conn = Some(ConnState::Error);
                                rt.error = Some(format!("signer stopped: {e}"));
                            }
                        }
                        r
                    }));
                    started += 1;
                }
                Err(e) => {
                    // Friendly reason for the common case (wrong passphrase).
                    bot.unlock_error = Some(match e {
                        keystore::KeystoreError::BadPassphrase => "wrong passphrase".into(),
                        other => other.to_string(),
                    });
                }
            }
        }
        started
    }

    /// Flip the active client's pause flag. This is the REAL pause: the
    /// shared atomic is read by the daemon's poll loop, which stops
    /// claiming/signing/submitting while set. We also set the conn pill
    /// immediately for instant feedback (the daemon confirms it on its
    /// next tick).
    pub fn toggle_pause_active(&mut self) {
        if let Some(bot) = self.bots.get(self.active_bot_idx) {
            let now_paused = !bot.is_paused();
            if let Ok(mut p) = bot.pause.lock() {
                *p = now_paused;
            }
            if let Ok(mut rt) = bot.runtime.lock() {
                rt.conn = Some(if now_paused {
                    ConnState::Paused
                } else {
                    // Resume: daemon will flip to Ready on its next poll.
                    ConnState::Connecting
                });
            }
        }
    }

    /// Whether the in-process signing daemon is currently running.
    pub fn daemon_running(&self) -> bool {
        // In Hub mode, we could return true if ANY daemon is running,
        // or just rely on individual bot statuses. We'll say true if any are running.
        self.bots
            .iter()
            .any(|b| b.daemon_handle.as_ref().is_some_and(|h| !h.is_finished()))
    }

    pub fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> AppOutcome {
        // Modal traps every keystroke until dismissed.
        if self.modal.is_some() {
            return self.on_key_modal(code, mods);
        }

        // If editing a setting, route everything to the active tab handler first
        // and suppress global navigation/quit shortcuts (except Esc to cancel).
        if self.tab == Tab::Settings && self.settings.is_editing {
            if code == KeyCode::Esc {
                self.settings.is_editing = false;
                return AppOutcome::Continue;
            }
            return screens::settings::handle_key(self, code, mods);
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
                self.tab = Tab::Solana;
                AppOutcome::Continue
            }
            (KeyCode::Char('4'), _) => {
                self.tab = Tab::Clients;
                self.clients.reload();
                AppOutcome::Continue
            }
            (KeyCode::Char('5'), _) => {
                self.tab = Tab::Settings;
                AppOutcome::Continue
            }
            (KeyCode::Char('6'), _) => {
                self.tab = Tab::Logs;
                AppOutcome::Continue
            }
            (KeyCode::Char('p'), _) if self.tab == Tab::Status => {
                self.toggle_pause_active();
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
                        let just_unlocked = self.unlock_locked_bots(&input);

                        // Any bots still locked? Re-prompt, naming the next one
                        // so the operator knows EXACTLY which client needs a
                        // (possibly different) passphrase — never silently skip.
                        let still_locked: Vec<String> = self
                            .bots
                            .iter()
                            .filter(|b| !b.is_running())
                            .map(|b| match &b.unlock_error {
                                Some(e) => format!("{} ({e})", b.name),
                                None => b.name.clone(),
                            })
                            .collect();

                        if still_locked.is_empty() {
                            self.modal = Some(Modal::Message {
                                title: "Signer Hub started".into(),
                                body: format!(
                                    "All {} client(s) unlocked and started. Watch the Status and Logs tabs.",
                                    self.bots.len()
                                ),
                            });
                        } else if just_unlocked > 0 {
                            // Progress made — prompt for the NEXT locked bot's
                            // passphrase (per-bot passphrases supported).
                            self.modal = Some(Modal::Unlock {
                                input: String::new(),
                                error: Some(format!(
                                    "Unlocked {just_unlocked}. Still locked: {}. Enter its passphrase:",
                                    still_locked.join(", ")
                                )),
                            });
                        } else {
                            // No progress at all — wrong passphrase for the
                            // remaining bot(s).
                            self.modal = Some(Modal::Unlock {
                                input: String::new(),
                                error: Some(format!(
                                    "Could not unlock: {}. Wrong passphrase?",
                                    still_locked.join(", ")
                                )),
                            });
                        }
                        let _ = input;
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
                KeyCode::Enter => {
                    if let Some(bot) = self.bots.get(self.active_bot_idx) {
                        match keystore::decrypt(&bot.ks_path, input.as_bytes()) {
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
                        }
                    } else {
                        AppOutcome::Continue
                    }
                }
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
                        if let Some(bot) = self.bots.get_mut(self.active_bot_idx) {
                            let _ = std::fs::remove_file(&bot.ks_path);
                            bot.agent_address = None;
                        }
                        AppOutcome::Continue
                    }
                    ConfirmAction::RemoveClient { id } => {
                        self.remove_vault_client(&id);
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
            Modal::Totp {
                bot_idx,
                challenge_id,
                expires_at,
                mut input,
            } => match code {
                KeyCode::Esc => {
                    // Operator declined — remember the challenge so the
                    // tick loop doesn't immediately re-raise the modal;
                    // the daemon's bounded wait fails the ack on its own.
                    self.totp_dismissed = Some(challenge_id);
                    AppOutcome::Continue
                }
                KeyCode::Enter => {
                    if let Some(bot) = self.bots.get(bot_idx) {
                        if let Ok(mut g) = bot.hl_runtime.totp_answer.lock() {
                            *g = Some(input.trim().to_string());
                        }
                    }
                    self.totp_dismissed = Some(challenge_id);
                    AppOutcome::Continue
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::Totp {
                        bot_idx,
                        challenge_id,
                        expires_at,
                        input,
                    });
                    AppOutcome::Continue
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    input.push(c);
                    self.modal = Some(Modal::Totp {
                        bot_idx,
                        challenge_id,
                        expires_at,
                        input,
                    });
                    AppOutcome::Continue
                }
                _ => {
                    self.modal = Some(Modal::Totp {
                        bot_idx,
                        challenge_id,
                        expires_at,
                        input,
                    });
                    AppOutcome::Continue
                }
            },
            Modal::SolUnlock { mut input, .. } => match code {
                KeyCode::Esc => AppOutcome::Continue,
                KeyCode::Enter => {
                    match self.sol.unlock(&input) {
                        Ok(()) => {
                            self.modal = Some(Modal::Message {
                                title: "Solana signer started".into(),
                                body: "Keystore unlocked — sell/copy streams connecting and the \
                                       :5829 web bridge is serving. Watch the Solana tab."
                                    .into(),
                            });
                        }
                        Err(e) => {
                            self.modal = Some(Modal::SolUnlock {
                                input: String::new(),
                                error: Some(e),
                            });
                        }
                    }
                    AppOutcome::Continue
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::SolUnlock { input, error: None });
                    AppOutcome::Continue
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.modal = Some(Modal::SolUnlock { input, error: None });
                    AppOutcome::Continue
                }
                _ => {
                    self.modal = Some(Modal::SolUnlock { input, error: None });
                    AppOutcome::Continue
                }
            },
            Modal::SolBudget { mut input, .. } => match code {
                KeyCode::Esc => AppOutcome::Continue,
                KeyCode::Enter => {
                    let trimmed = input.trim().to_string();
                    let parsed: Result<Option<f64>, String> = if trimmed.is_empty() {
                        Ok(None)
                    } else {
                        trimmed
                            .parse::<f64>()
                            .map_err(|_| "not a number".to_string())
                            .and_then(|v| {
                                if v.is_finite() && v > 0.0 {
                                    Ok(Some(v))
                                } else {
                                    Err("must be > 0 (or empty to disarm)".into())
                                }
                            })
                    };
                    match parsed.and_then(|v| self.sol.set_budget(v).map(|()| v)) {
                        Ok(v) => {
                            self.modal = Some(Modal::Message {
                                title: "Copy budget saved".into(),
                                body: match v {
                                    Some(s) => format!(
                                        "Session budget set to {s} SOL. Applied immediately \
                                         (running stream restarted)."
                                    ),
                                    None => "Budget cleared — copy buys are DISARMED. TP/SL \
                                             and mirror sells keep running."
                                        .into(),
                                },
                            });
                        }
                        Err(e) => {
                            self.modal = Some(Modal::SolBudget {
                                input: trimmed,
                                error: Some(e),
                            });
                        }
                    }
                    AppOutcome::Continue
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::SolBudget { input, error: None });
                    AppOutcome::Continue
                }
                KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
                    input.push(c);
                    self.modal = Some(Modal::SolBudget { input, error: None });
                    AppOutcome::Continue
                }
                _ => {
                    self.modal = Some(Modal::SolBudget { input, error: None });
                    AppOutcome::Continue
                }
            },
            Modal::ClientLabel { id, mut input, .. } => match code {
                KeyCode::Esc => AppOutcome::Continue,
                KeyCode::Enter => {
                    let label = {
                        let t = input.trim();
                        (!t.is_empty()).then(|| t.to_string())
                    };
                    let res = crate::clients::open_vault()
                        .and_then(|v| v.ok_or_else(|| "no vault on this device".into()))
                        .and_then(|mut v| {
                            v.set_label(&id, label.clone()).map_err(|e| e.to_string())
                        });
                    match res {
                        Ok(()) => {
                            // Mirror into the live bot name (vault HL bots
                            // are named after their label).
                            if let Some(l) = &label {
                                for bot in self.bots.iter_mut() {
                                    if bot.vault_id.as_deref() == Some(id.as_str()) {
                                        bot.name = l.clone();
                                    }
                                }
                            }
                            self.clients.reload();
                        }
                        Err(e) => {
                            self.modal = Some(Modal::ClientLabel {
                                id,
                                input,
                                error: Some(e),
                            });
                        }
                    }
                    AppOutcome::Continue
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::ClientLabel {
                        id,
                        input,
                        error: None,
                    });
                    AppOutcome::Continue
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.modal = Some(Modal::ClientLabel {
                        id,
                        input,
                        error: None,
                    });
                    AppOutcome::Continue
                }
                _ => {
                    self.modal = Some(Modal::ClientLabel {
                        id,
                        input,
                        error: None,
                    });
                    AppOutcome::Continue
                }
            },
            Modal::ClientAdd {
                step,
                mut label,
                mut input,
                error,
            } => match code {
                KeyCode::Esc => AppOutcome::Continue,
                KeyCode::Enter => {
                    if step == 0 {
                        label = input.trim().to_string();
                        self.modal = Some(Modal::ClientAdd {
                            step: 1,
                            label,
                            input: String::new(),
                            error: None,
                        });
                        return AppOutcome::Continue;
                    }
                    let password = input.clone();
                    if password.len() < 8 && !crate::clients::vault_exists() {
                        self.modal = Some(Modal::ClientAdd {
                            step,
                            label,
                            input: String::new(),
                            error: Some("master password must be at least 8 characters".into()),
                        });
                        return AppOutcome::Continue;
                    }
                    let lbl = (!label.is_empty()).then(|| label.clone());
                    match crate::clients::client_add_sol(lbl, &password) {
                        Ok(entry) => {
                            self.clients.reload();
                            self.modal = Some(Modal::Message {
                                title: "Solana wallet added".into(),
                                body: format!(
                                    "{} is in the vault. The runtime picks it up on the next \
                                     unlock; set it primary on this tab to make it the executor.",
                                    entry.address
                                ),
                            });
                        }
                        Err(e) => {
                            self.modal = Some(Modal::ClientAdd {
                                step,
                                label,
                                input: String::new(),
                                error: Some(e),
                            });
                        }
                    }
                    AppOutcome::Continue
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.modal = Some(Modal::ClientAdd {
                        step,
                        label,
                        input,
                        error,
                    });
                    AppOutcome::Continue
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    self.modal = Some(Modal::ClientAdd {
                        step,
                        label,
                        input,
                        error,
                    });
                    AppOutcome::Continue
                }
                _ => {
                    self.modal = Some(Modal::ClientAdd {
                        step,
                        label,
                        input,
                        error,
                    });
                    AppOutcome::Continue
                }
            },
            Modal::ClientImport {
                mut step,
                chain,
                mut secret,
                mut label,
                mut input,
                error,
            } => {
                use degenbox_signer_core::WalletChain;
                match code {
                    KeyCode::Esc => AppOutcome::Continue,
                    // Step 0: chain choice.
                    KeyCode::Char('s') | KeyCode::Char('S') if step == 0 => {
                        self.modal = Some(Modal::ClientImport {
                            step: 1,
                            chain: WalletChain::Sol,
                            secret,
                            label,
                            input: String::new(),
                            error: None,
                        });
                        AppOutcome::Continue
                    }
                    KeyCode::Char('h') | KeyCode::Char('H') if step == 0 => {
                        self.modal = Some(Modal::ClientImport {
                            step: 1,
                            chain: WalletChain::Hl,
                            secret,
                            label,
                            input: String::new(),
                            error: None,
                        });
                        AppOutcome::Continue
                    }
                    KeyCode::Enter if step > 0 => {
                        match step {
                            1 => {
                                if input.trim().is_empty() {
                                    self.modal = Some(Modal::ClientImport {
                                        step,
                                        chain,
                                        secret,
                                        label,
                                        input,
                                        error: Some("paste the private key first".into()),
                                    });
                                    return AppOutcome::Continue;
                                }
                                secret = input.trim().to_string();
                                step = 2;
                                input = String::new();
                            }
                            2 => {
                                label = input.trim().to_string();
                                step = 3;
                                input = String::new();
                            }
                            _ => {
                                let password = input.clone();
                                let lbl = (!label.is_empty()).then(|| label.clone());
                                match crate::clients::client_import(chain, &secret, lbl, &password)
                                {
                                    Ok(entry) => {
                                        self.clients.reload();
                                        if entry.chain == WalletChain::Hl {
                                            self.append_vault_bot(&entry.id);
                                        }
                                        self.modal = Some(Modal::Message {
                                            title: "Wallet imported".into(),
                                            body: format!(
                                                "{} ({}) is in the vault.{}",
                                                entry.address,
                                                entry.chain.as_str(),
                                                if entry.chain == WalletChain::Hl {
                                                    " Unlock ([u] on Status) to start its \
                                                     runtime; pair it via `register` if new."
                                                } else {
                                                    " The runtime picks it up on the next unlock."
                                                }
                                            ),
                                        });
                                        return AppOutcome::Continue;
                                    }
                                    Err(e) => {
                                        self.modal = Some(Modal::ClientImport {
                                            step,
                                            chain,
                                            secret,
                                            label,
                                            input: String::new(),
                                            error: Some(e),
                                        });
                                        return AppOutcome::Continue;
                                    }
                                }
                            }
                        }
                        self.modal = Some(Modal::ClientImport {
                            step,
                            chain,
                            secret,
                            label,
                            input,
                            error: None,
                        });
                        AppOutcome::Continue
                    }
                    KeyCode::Backspace if step > 0 => {
                        input.pop();
                        self.modal = Some(Modal::ClientImport {
                            step,
                            chain,
                            secret,
                            label,
                            input,
                            error,
                        });
                        AppOutcome::Continue
                    }
                    KeyCode::Char(c) if step > 0 => {
                        input.push(c);
                        self.modal = Some(Modal::ClientImport {
                            step,
                            chain,
                            secret,
                            label,
                            input,
                            error,
                        });
                        AppOutcome::Continue
                    }
                    _ => {
                        self.modal = Some(Modal::ClientImport {
                            step,
                            chain,
                            secret,
                            label,
                            input,
                            error,
                        });
                        AppOutcome::Continue
                    }
                }
            }
        }
    }

    /// Append a live (locked) `BotContext` for a vault HL wallet that
    /// was just imported, so it can be unlocked without a restart.
    fn append_vault_bot(&mut self, vault_id: &str) {
        use degenbox_signer_core::WalletChain;
        let Ok(Some(vault)) = crate::clients::open_vault() else {
            return;
        };
        let Some(entry) = vault.get(vault_id).cloned() else {
            return;
        };
        if entry.chain != WalletChain::Hl
            || self
                .bots
                .iter()
                .any(|b| b.vault_id.as_deref() == Some(vault_id))
        {
            return;
        }
        let is_primary = vault
            .primary(WalletChain::Hl)
            .is_some_and(|w| w.id == entry.id);
        self.bots
            .push(vault_bot_context(&vault, &entry, is_primary));
    }

    /// Remove a vault client: stop its live runtime (if any), then drop
    /// it from the vault (keystore preserved as `.removed.bak`).
    fn remove_vault_client(&mut self, id: &str) {
        if let Some(pos) = self
            .bots
            .iter()
            .position(|b| b.vault_id.as_deref() == Some(id))
        {
            let bot = self.bots.remove(pos);
            bot.hl_runtime
                .daemon_running
                .store(false, std::sync::atomic::Ordering::SeqCst);
            if self.active_bot_idx >= self.bots.len() {
                self.active_bot_idx = self.bots.len().saturating_sub(1);
            }
        }
        let res = crate::clients::open_vault()
            .and_then(|v| v.ok_or_else(|| "no vault on this device".into()))
            .and_then(|mut v| v.remove(id).map_err(|e| e.to_string()));
        self.clients.reload();
        self.modal = Some(match res {
            Ok(entry) => Modal::Message {
                title: "Client removed".into(),
                body: format!(
                    "{} removed from the vault. Its encrypted keystore is preserved as \
                     {}.removed.bak.",
                    entry.address, entry.file
                ),
            },
            Err(e) => Modal::Message {
                title: "Remove failed".into(),
                body: e,
            },
        });
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

    pub fn is_unlocked(&self) -> bool {
        self.bots.iter().any(|b| b.agent_address.is_some())
    }

    pub fn unacked_queue_size(&self) -> usize {
        self.bots
            .iter()
            .map(|b| b.runtime.lock().map(|r| r.queue.pending).unwrap_or(0))
            .sum()
    }

    fn render_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let active = self.bots.get(self.active_bot_idx);
        let conn = active
            .and_then(|b| b.runtime.lock().ok())
            .and_then(|r| r.conn)
            .unwrap_or(ConnState::Offline);
        let update = self.bots.iter().find_map(|b| {
            b.runtime
                .lock()
                .ok()
                .and_then(|r| r.update_available.clone())
        });
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
            Tab::Solana => screens::solana::render(self, frame, area),
            Tab::Clients => screens::clients::render(self, frame, area),
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
    "[Tab] tabs  [↑/↓] select client  [p] pause  [u] unlock  [1-6] jump  [q] quit"
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
    fn pause_toggles_real_atomic_and_conn_state() {
        let mut app = App::for_test(Config::default(), RuntimeState::new());
        app.tab = Tab::Status;
        // Force the active bot's runtime to Ready first so the first toggle
        // is unambiguously Ready → Paused.
        {
            let bot = &app.bots[app.active_bot_idx];
            bot.runtime.lock().unwrap().conn = Some(ConnState::Ready);
        }
        // 'p' must flip the SHARED pause atomic (the thing the daemon
        // actually reads) — not just a display field.
        app.on_key(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(
            app.bots[app.active_bot_idx].is_paused(),
            "pause atomic must be set"
        );
        assert_eq!(
            app.bots[app.active_bot_idx]
                .runtime
                .lock()
                .unwrap()
                .conn
                .unwrap(),
            ConnState::Paused
        );
        // Toggling again resumes: atomic cleared, pill goes to Connecting
        // (the daemon flips it to Ready on its next poll).
        app.on_key(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(
            !app.bots[app.active_bot_idx].is_paused(),
            "pause atomic must be cleared on resume"
        );
        assert_eq!(
            app.bots[app.active_bot_idx]
                .runtime
                .lock()
                .unwrap()
                .conn
                .unwrap(),
            ConnState::Connecting
        );
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
                pnl: None,
            });
        }
        assert_eq!(rt.orders.len(), 50);
        assert_eq!(rt.orders.front().unwrap().size_usd, "10");
    }
}
