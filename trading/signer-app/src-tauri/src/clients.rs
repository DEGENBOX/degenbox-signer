//! Multi-wallet client management — the vault-backed "clients" surface.
//!
//! One user → N wallets ("clients"): multiple Solana AND multiple
//! Hyperliquid wallets, all keys in the vault under ONE master
//! password (`degenbox_signer_core::vault`). This module owns:
//!
//! - vault helpers (open / primary resolution / legacy fallbacks),
//! - the per-client pause gates (effective = global kill-switch OR
//!   per-client flag),
//! - runtime orchestration — TRUE per-wallet executors:
//!   * HL: every unlocked, paired wallet runs its OWN poll/sign/report
//!     daemon claiming ONLY its wallet (`?wallet=` +
//!     [`degenbox_signer_core::hl::daemon::ClaimScope`] belt), with its
//!     own exec-state/executed ledger, telemetry, and pause gate. The
//!     designated primary additionally owns legacy unstamped rows.
//!     Secondaries first PROBE the gateway for wallet scoping
//!     (`GET /api/trading/clients`) and fall back to the heartbeat+
//!     balance standby loop on pre-multi-client gateways — an ignored
//!     `?wallet=` filter on an old gateway must never let a secondary
//!     claim (and mis-execute) another wallet's instruction.
//!   * Sol: ONE dispatcher consuming the two user-scoped streams once,
//!     fanned out to one engine per wallet (`sol::runtime`) — events
//!     route by `wallet_pubkey`, unstamped legacy events go to the
//!     primary only.
//!
//!   Resource topology: N HL daemons → N pending-polls + N balance
//!   loops (per-wallet HTTP, no shared state); Sol → 2 websockets per
//!   device TOTAL, N engines in one task.
//!
//! - the gateway `/api/trading/clients` client (graceful-degrade: a
//!   404 means the endpoint hasn't shipped — local-only view),
//! - the `clients_*` IPC commands incl. per-client budget /
//!   active-config / preset-assignment pass-throughs.

use crate::hl::config::HlConfig;
use crate::hl::daemon::CoreClaimScope;
use crate::hl::runtime::{ConnState, HlRuntime, SharedHlRuntime};
use crate::state::{AppState, ClientHandle, ClientRole};
use degenbox_signer_core as core;
use degenbox_signer_core::{Vault, WalletChain, WalletEntry};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{Manager, State};

// ─── vault helpers ──────────────────────────────────────────────────

pub fn vault_dir() -> Result<PathBuf, String> {
    core::default_vault_dir().map_err(|e| e.to_string())
}

/// Open the vault if one exists. `Ok(None)` pre-migration.
pub fn open_vault() -> Result<Option<Vault>, String> {
    let dir = vault_dir()?;
    if !Vault::exists(&dir) {
        return Ok(None);
    }
    Vault::open(&dir).map(Some).map_err(|e| e.to_string())
}

/// Primary Solana pubkey: vault primary, else the legacy single
/// keystore's pubkey (pre-migration installs).
pub fn primary_sol_pubkey() -> Option<String> {
    if let Ok(Some(v)) = open_vault() {
        if let Some(w) = v.primary(WalletChain::Sol) {
            return Some(w.address.clone());
        }
    }
    let path = core::sol_keystore_path().ok()?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<core::Keystore>(&bytes)
        .ok()
        .map(|ks| ks.pubkey)
}

/// Primary HL agent address: vault primary, else the legacy keystore.
pub fn primary_hl_address() -> Option<String> {
    if let Ok(Some(v)) = open_vault() {
        if let Some(w) = v.primary(WalletChain::Hl) {
            return Some(w.address.clone());
        }
    }
    let path = core::hl_keystore_path().ok()?;
    if !path.exists() {
        return None;
    }
    core::hl_peek_address(&path).ok()
}

pub fn has_any_wallet(chain: WalletChain) -> bool {
    if let Ok(Some(v)) = open_vault() {
        if v.wallets().iter().any(|w| w.chain == chain) {
            return true;
        }
    }
    let legacy = match chain {
        WalletChain::Sol => core::sol_keystore_path(),
        WalletChain::Hl => core::hl_keystore_path(),
    };
    legacy.map(|p| p.exists()).unwrap_or(false)
}

/// Open-or-create the vault under `password` and adopt any legacy
/// single-file keystores. The shared mutation entry point for the
/// wizard append commands AND unlock.
pub fn open_or_create_vault_migrated(password: &str) -> Result<Vault, String> {
    let dir = vault_dir()?;
    let mut vault = Vault::open_or_create(&dir, password).map_err(|e| e.to_string())?;
    vault.verify_password(password).map_err(|e| e.to_string())?;
    let sol_legacy = core::sol_keystore_path().map_err(|e| e.to_string())?;
    let hl_legacy = core::hl_keystore_path().map_err(|e| e.to_string())?;
    let hl_cfg = core::hl_config_path().map_err(|e| e.to_string())?;
    let report = vault
        .migrate_legacy(&sol_legacy, &hl_legacy, Some(&hl_cfg), password)
        .map_err(|e| format!("legacy keystore migration: {e}"))?;
    if report.migrated_sol.is_some() || report.migrated_hl.is_some() {
        tracing::info!(
            sol = ?report.migrated_sol,
            hl = ?report.migrated_hl,
            "migrated legacy keystores into the vault (originals kept as .bak)"
        );
    }
    for note in &report.notes {
        tracing::warn!(%note, "vault migration note");
    }
    Ok(vault)
}

/// Executed-marker ledger path for a vault HL wallet. Seeded ONCE from
/// the legacy global `executed.jsonl` (when present) so the vault
/// migration can never reopen the re-submit window for an instruction
/// whose `post_result` is still retrying.
pub fn hl_executed_path_seeded(vault: &Vault, entry: &WalletEntry) -> PathBuf {
    let p = vault.hl_executed_path(entry);
    if !p.exists() {
        if let Ok(global) = degenbox_signer_core::hl::config::executed_path() {
            if global.exists() {
                if let Err(e) = std::fs::copy(&global, &p) {
                    tracing::warn!(error = %e, "could not seed per-wallet executed ledger from the global one");
                }
            }
        }
    }
    p
}

/// The HL pairing config for a wallet: its per-wallet vault config when
/// present, else (primary only) the legacy global `hl-config.json` the
/// CLI signer shares.
pub fn hl_config_for(vault: &Vault, entry: &WalletEntry, is_primary: bool) -> HlConfig {
    let per_wallet = vault.hl_config_path(entry);
    if per_wallet.exists() {
        return HlConfig::load_from(&per_wallet);
    }
    if is_primary {
        return HlConfig::load_or_default();
    }
    HlConfig::default()
}

// ─── pause gates ────────────────────────────────────────────────────

/// Recompute every client's effective pause gate
/// (= global kill-switch OR per-client flag). Cheap; called on every
/// pause toggle and on unlock.
pub fn recompute_pause_gates(state: &AppState) {
    let global = state.paused.lock().map(|g| *g).unwrap_or(false);
    if let Ok(clients) = state.clients.lock() {
        for c in clients.iter() {
            if let Ok(mut g) = c.pause_gate.lock() {
                *g = global || c.entry.paused;
            }
        }
    }
}

// ─── runtime orchestration ──────────────────────────────────────────

/// Bring every unlocked client's runtime online (idempotent).
///
/// Topology (true per-wallet executors — see module docs):
/// - HL primary    → full daemon, claim scope `Scoped{master,
///   allow_unstamped}` (global telemetry mirror).
/// - HL secondary  → capability probe, then full daemon with a STRICT
///   per-wallet claim scope — or the register+balance standby loop on
///   pre-multi-client gateways / unpaired wallets.
/// - Sol           → ONE dispatcher, one engine per unlocked wallet
///   (`sol::runtime::spawn` builds the slots itself).
pub fn start_runtimes(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let vault = match open_vault() {
        Ok(Some(v)) => v,
        _ => {
            // Legacy pre-vault unlock path — Sol engine only.
            crate::sol::runtime::spawn(app);
            return;
        }
    };
    struct HlSpawnPlan {
        entry: WalletEntry,
        role: ClientRole,
        hl_secret: Option<String>,
        pause_gate: Arc<Mutex<bool>>,
        hl_runtime: Option<SharedHlRuntime>,
        hl_executor: Arc<AtomicBool>,
    }
    let snapshot: Vec<HlSpawnPlan> = match state.clients.lock() {
        Ok(clients) => clients
            .iter()
            .map(|c| HlSpawnPlan {
                entry: c.entry.clone(),
                role: c.role,
                hl_secret: c.hl_secret_hex.clone(),
                pause_gate: c.pause_gate.clone(),
                hl_runtime: c.hl_runtime.clone(),
                hl_executor: c.hl_executor.clone(),
            })
            .collect(),
        Err(_) => return,
    };

    for HlSpawnPlan {
        entry,
        role,
        hl_secret,
        pause_gate,
        hl_runtime,
        hl_executor,
    } in snapshot
    {
        if entry.chain != WalletChain::Hl {
            continue;
        }
        let Some(secret) = hl_secret else { continue };
        start_hl_runtime(
            app,
            &vault,
            &entry,
            role,
            secret,
            pause_gate,
            hl_runtime,
            hl_executor,
        );
    }

    // Sol dispatcher — `spawn` builds one engine per unlocked wallet
    // and checks the global kill-switch itself.
    crate::sol::runtime::spawn(app);
}

/// Bring ONE HL wallet's runtime online from its live plumbing —
/// primary → full daemon (when paired) on the shared device runtime;
/// standby → capability-probed scoped daemon / standby loop. Shared by
/// the unlock-time orchestration ([`start_runtimes`]) and the
/// incremental add path ([`client_activate`]) so a freshly added wallet
/// gets exactly the same topology a relock/unlock would build.
#[allow(clippy::too_many_arguments)]
fn start_hl_runtime(
    app: &tauri::AppHandle,
    vault: &Vault,
    entry: &WalletEntry,
    role: ClientRole,
    secret: String,
    pause_gate: Arc<Mutex<bool>>,
    hl_runtime: Option<SharedHlRuntime>,
    hl_executor: Arc<AtomicBool>,
) {
    let state = app.state::<AppState>();
    let cfg = hl_config_for(vault, entry, role == ClientRole::Primary);
    let paired = cfg.api_token.is_some() && cfg.agent_address.is_some();
    match role {
        ClientRole::Primary => {
            if paired {
                let executed = hl_executed_path_seeded(vault, entry);
                hl_executor.store(true, Ordering::SeqCst);
                let scope = crate::commands::primary_claim_scope(&cfg);
                crate::commands::spawn_hl_daemon_with(
                    app,
                    secret,
                    cfg,
                    state.hl_runtime.clone(),
                    pause_gate,
                    Some(executed),
                    scope,
                );
            } else {
                hl_executor.store(false, Ordering::SeqCst);
                state.hl_runtime.set_conn(ConnState::Offline);
            }
        }
        ClientRole::Standby => {
            if let Some(rt) = hl_runtime {
                let executed = hl_executed_path_seeded(vault, entry);
                spawn_hl_secondary(
                    app,
                    entry.clone(),
                    cfg,
                    secret,
                    rt,
                    pause_gate,
                    executed,
                    hl_executor,
                );
            }
        }
    }
}

/// Stop every client runtime + wipe per-client secrets. Used by lock
/// and before a role-changing restart.
pub fn stop_runtimes(state: &AppState) {
    if let Ok(mut clients) = state.clients.lock() {
        for c in clients.iter_mut() {
            if let Some(rt) = &c.hl_runtime {
                rt.daemon_running.store(false, Ordering::SeqCst);
                rt.set_conn(ConnState::Offline);
            }
        }
    }
    state
        .hl_runtime
        .daemon_running
        .store(false, Ordering::SeqCst);
    state.hl_runtime.set_conn(ConnState::Offline);
    crate::sol::runtime::stop(state);
}

/// Stop + restart all runtimes (primary swap / pause change that needs
/// a respawn). The HL daemon honours the `daemon_running=false` signal
/// on its next tick; the CAS guard in the spawn path waits it out.
pub fn restart_runtimes(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    stop_runtimes(&state);
    recompute_pause_gates(&state);
    start_runtimes(app);
}

/// Rebuild the in-memory client list from the vault using the master
/// password (full unlock). Replaces any previous list.
pub fn unlock_clients(state: &AppState, vault: &Vault, password: &str) -> Result<(), String> {
    let primary_sol = vault.primary(WalletChain::Sol).map(|w| w.id.clone());
    let primary_hl = vault.primary(WalletChain::Hl).map(|w| w.id.clone());
    let mut handles: Vec<ClientHandle> = Vec::with_capacity(vault.wallets().len());
    for entry in vault.wallets() {
        let role =
            if Some(&entry.id) == primary_sol.as_ref() || Some(&entry.id) == primary_hl.as_ref() {
                ClientRole::Primary
            } else {
                ClientRole::Standby
            };
        let mut handle = ClientHandle {
            entry: entry.clone(),
            role,
            pause_gate: Arc::new(Mutex::new(entry.paused)),
            sol_seed: None,
            hl_secret_hex: None,
            hl_runtime: None,
            sol_runtime: None,
            hl_executor: Arc::new(AtomicBool::new(false)),
        };
        match entry.chain {
            WalletChain::Sol => {
                let kp = vault
                    .unlock_sol(&entry.id, password)
                    .map_err(|e| format!("unlock {} ({}): {e}", entry.address, entry.id))?;
                let mut seed = [0u8; 32];
                seed.copy_from_slice(kp.secret_bytes().as_slice());
                handle.sol_seed = Some(seed);
                handle.sol_runtime = Some(if role == ClientRole::Primary {
                    state.sol_runtime.clone()
                } else {
                    Arc::new(crate::sol::runtime::SolRuntimeInner::default())
                });
                drop(kp);
            }
            WalletChain::Hl => {
                let (secret_hex, _addr) = vault
                    .unlock_hl(&entry.id, password)
                    .map_err(|e| format!("unlock {} ({}): {e}", entry.address, entry.id))?;
                handle.hl_secret_hex = Some(secret_hex);
                handle.hl_runtime = Some(if role == ClientRole::Primary {
                    state.hl_runtime.clone()
                } else {
                    Arc::new(HlRuntime::default())
                });
            }
        }
        handles.push(handle);
    }
    let mut g = state.clients.lock().map_err(|e| e.to_string())?;
    *g = handles;
    Ok(())
}

// ─── HL secondary executor (probe → scoped daemon | standby) ───────

/// Does this gateway scope the HL claim queue per wallet?
///
/// There is no direct capability endpoint; `GET /api/trading/clients`
/// shipped in the SAME backend slice as the `?wallet=` claim filter +
/// `target_wallet` stamping, so its existence is the deploy proxy:
/// `Ok(true)` = wallet scoping live, `Ok(false)` = 404/405 (older
/// gateway), `Err` = undetermined (network/auth) — the caller retries,
/// then falls back to standby. This gate matters because an old
/// gateway silently IGNORES the unknown `?wallet=` param: a secondary
/// daemon polling it would claim other wallets' instructions.
async fn gateway_supports_wallet_scoping(cfg: &HlConfig) -> Result<bool, String> {
    let token = cfg.api_token.clone().ok_or("not paired")?;
    let url = format!(
        "{}/api/trading/clients",
        cfg.server_url.trim_end_matches('/')
    );
    let resp = http()
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("capability probe: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 404 || status.as_u16() == 405 {
        return Ok(false);
    }
    if status.is_success() {
        return Ok(true);
    }
    Err(format!("capability probe: gateway {status}"))
}

/// Spawn the runtime for a NON-PRIMARY HL wallet.
///
/// Paired + multi-client gateway → a FULL poll/sign/report daemon with
/// a STRICT per-wallet claim scope (`?wallet=master`, unstamped rows
/// refused — those belong to the primary), its own executed ledger,
/// telemetry, and pause gate. Unpaired, or the gateway predates wallet
/// scoping → the register-heartbeat + balance standby loop (never
/// claims). One task per wallet, guarded by the same
/// `daemon_running` CAS + `run_generation` handshake as the primary.
#[allow(clippy::too_many_arguments)]
fn spawn_hl_secondary(
    app: &tauri::AppHandle,
    entry: WalletEntry,
    cfg: HlConfig,
    secret_hex: String,
    runtime: SharedHlRuntime,
    pause_gate: Arc<Mutex<bool>>,
    executed: PathBuf,
    hl_executor: Arc<AtomicBool>,
) {
    if runtime
        .daemon_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    // Same generation handshake as the primary daemon — a stale loop
    // that missed the stop window exits on the mismatch.
    let my_generation = runtime.run_generation.fetch_add(1, Ordering::SeqCst) + 1;
    hl_executor.store(false, Ordering::SeqCst);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let paired = cfg.api_token.is_some() && cfg.agent_address.is_some();
        let master = cfg.account_address.clone();
        let mut run_full_daemon = false;
        if paired && master.is_some() {
            // Probe (bounded retries on undetermined) — a restart
            // (unlock / pause toggle / primary change) re-probes.
            for attempt in 0..3u8 {
                if !runtime.daemon_running.load(Ordering::Relaxed)
                    || runtime.run_generation.load(Ordering::SeqCst) != my_generation
                {
                    runtime.set_conn(ConnState::Offline);
                    return;
                }
                match gateway_supports_wallet_scoping(&cfg).await {
                    Ok(supported) => {
                        run_full_daemon = supported;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(wallet = %entry.address, error = %e,
                            "wallet-scoping capability probe failed — retrying");
                        if attempt == 2 {
                            runtime.set_error(Some(format!(
                                "could not verify gateway wallet scoping ({e}) — running standby; resume/re-pair to retry"
                            )));
                        } else {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            }
        }

        if run_full_daemon {
            let master = master.expect("checked above");
            hl_executor.store(true, Ordering::SeqCst);
            tracing::info!(wallet = %entry.address, %master,
                "secondary HL wallet: gateway scopes claims per wallet — starting full executor");
            let opts = crate::hl::daemon::DaemonOpts {
                poll_interval: std::time::Duration::from_secs(cfg.poll_secs.max(1)),
                paper_mode: cfg.paper_mode,
                agent_address: entry.address.clone(),
                secret_hex,
                pause: pause_gate,
                runtime: runtime.clone(),
                app: app.clone(),
                executed_path: Some(executed),
                // STRICT scope: unstamped rows are the primary's.
                claim_scope: CoreClaimScope::Scoped {
                    wallet: master,
                    allow_unstamped: false,
                },
                config: cfg,
            };
            if let Err(e) = crate::hl::daemon::run(opts).await {
                tracing::error!(error = %e, wallet = %entry.address, "secondary HL daemon exited with error");
                runtime.set_conn(ConnState::Error);
                runtime.set_error(Some(format!("daemon stopped: {e}")));
            }
            hl_executor.store(false, Ordering::SeqCst);
        } else {
            hl_standby_loop(&entry, &cfg, &runtime, my_generation).await;
        }

        // Loop exited (error or stop) — clear the running guard so a
        // later spawn can take over, ONLY if no newer spawn superseded
        // us (a stale generation must not relock the successor).
        if runtime.run_generation.load(Ordering::SeqCst) == my_generation {
            runtime.daemon_running.store(false, Ordering::SeqCst);
        }
    });
}

/// Standby loop for an HL wallet that cannot execute (unpaired, or the
/// gateway predates per-wallet claim scoping): keep the pairing
/// heartbeat alive + refresh the master-account balance into this
/// client's own telemetry. NEVER polls `instructions/pending`.
async fn hl_standby_loop(
    entry: &WalletEntry,
    cfg: &HlConfig,
    runtime: &SharedHlRuntime,
    my_generation: u64,
) {
    use crate::hl::server::{RegisterReq, ServerClient};
    use degenbox_signer_core::hl::config::NetworkChoice;
    use degenbox_signer_core::hl::info::HttpInfoClient;
    use platform_hl_exchange::Network;

    let network = match cfg.network {
        NetworkChoice::Mainnet => Network::Mainnet,
        NetworkChoice::Testnet => Network::Testnet,
    };
    let server = cfg
        .api_token
        .clone()
        .and_then(|t| ServerClient::new(cfg.server_url.clone(), t).ok());
    let info = HttpInfoClient::new(network).ok();
    if server.is_none() {
        runtime.set_error(Some(
            "standby — not paired yet (no instruction polling; pair this wallet to execute)".into(),
        ));
    }
    runtime.set_conn(ConnState::Connecting);
    if let Ok(mut g) = runtime.account_address.lock() {
        g.clone_from(&cfg.account_address);
    }
    if let Ok(mut g) = runtime.agent_address.lock() {
        *g = Some(entry.address.clone());
    }
    let mut tick: u64 = 0;
    loop {
        if !runtime.daemon_running.load(Ordering::Relaxed)
            || runtime.run_generation.load(Ordering::SeqCst) != my_generation
        {
            runtime.set_conn(ConnState::Offline);
            return;
        }
        // Heartbeat register every 60 s (tick 0, 6, 12, …).
        if tick % 6 == 0 {
            if let Some(server) = &server {
                let req = RegisterReq {
                    agent_address: entry.address.clone(),
                    client_version: Some(format!(
                        "degenbox-signer-app {} (standby)",
                        env!("CARGO_PKG_VERSION")
                    )),
                    host_id: cfg.host_id.clone(),
                    paired_with_account: cfg.account_address.clone(),
                };
                match server.register(&req).await {
                    Ok(_) => {
                        runtime.set_conn(ConnState::Ready);
                        runtime.set_error(None);
                    }
                    Err(e) => {
                        runtime.set_conn(ConnState::Error);
                        runtime.set_error(Some(format!("standby register: {e}")));
                    }
                }
            }
        }
        // Balance refresh every tick (10 s) off the master account.
        if let (Some(info), Some(master)) = (&info, cfg.account_address.as_deref()) {
            match info.account_summary(master).await {
                Ok(summary) => {
                    let positions = summary
                        .positions
                        .iter()
                        .map(|p| crate::hl::runtime::PositionRow {
                            coin: p.coin.clone(),
                            szi: p.szi.normalize().to_string(),
                            side: if p.szi.is_sign_negative() {
                                "short".into()
                            } else {
                                "long".into()
                            },
                            unrealized_pnl: p.unrealized_pnl.clone(),
                            entry_px: p.entry_px.clone(),
                        })
                        .collect();
                    if let Ok(mut g) = runtime.balance.lock() {
                        *g = crate::hl::runtime::BalanceSnapshot {
                            account_value_usd: summary.account_value_usd,
                            withdrawable_usd: summary.withdrawable_usd,
                            spot_usdc: summary.spot_usdc,
                            is_unified: summary.is_unified,
                            unified_value_usd: summary.unified_value_usd,
                            positions,
                            fetched_at: Some(chrono::Utc::now()),
                            error: None,
                        };
                    }
                }
                Err(e) => {
                    if let Ok(mut g) = runtime.balance.lock() {
                        g.error = Some(format!("{e}"));
                    }
                }
            }
        }
        tick = tick.wrapping_add(1);
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

// ─── gateway clients API (graceful-degrade) ─────────────────────────

/// `/api/trading/clients` chain names ("hyperliquid"|"solana") ↔ the
/// vault's short names ("hl"|"sol").
fn gw_chain(chain: WalletChain) -> &'static str {
    match chain {
        WalletChain::Sol => "solana",
        WalletChain::Hl => "hyperliquid",
    }
}

/// Normalize a gateway chain name to the vault's short form for the
/// UI's merged view (unknown values pass through).
fn local_chain_name(gw: &str) -> String {
    match gw {
        "solana" => "sol".into(),
        "hyperliquid" => "hl".into(),
        other => other.into(),
    }
}

/// Per-client budget, verbatim from the gateway (`BudgetView`). USD
/// figures are decimal-as-string on the wire; lamports are numbers.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GatewayBudget {
    #[serde(default)]
    pub session_budget_usd: Option<serde_json::Value>,
    #[serde(default)]
    pub max_position_usd: Option<serde_json::Value>,
    #[serde(default)]
    pub default_size_usd: Option<serde_json::Value>,
    #[serde(default)]
    pub session_budget_lamports: Option<i64>,
    #[serde(default)]
    pub per_trade_lamports: Option<i64>,
}

/// The HL single-active-config slot (`ActiveConfigView`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayActiveConfig {
    /// "caller" | "copytrade"
    pub r#type: String,
    /// caller_id (text) for callers, copy-config uuid for copytrade.
    pub ref_id: String,
    #[serde(default)]
    pub since: Option<String>,
}

/// Sol assignment counts (`AssignmentCounts`); zeros for HL clients.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GatewayAssignments {
    #[serde(default)]
    pub presets: i64,
    #[serde(default)]
    pub copytrade: i64,
}

/// One row from `GET /api/trading/clients` (`ClientSummary`, verified
/// against `crates/modules/trading/src/api/clients.rs`). Lenient —
/// every field except `id` is defaulted so additive gateway changes
/// never break a deployed client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayClient {
    pub id: String,
    /// "hyperliquid" | "solana".
    #[serde(default)]
    pub chain: Option<String>,
    /// HL lowercase 0x…; Sol base58.
    #[serde(default)]
    pub wallet: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub paused: Option<bool>,
    #[serde(default)]
    pub budget: Option<GatewayBudget>,
    /// HL only; `None` = no active config (or a Sol client).
    #[serde(default)]
    pub active_config: Option<GatewayActiveConfig>,
    #[serde(default)]
    pub assignments: Option<GatewayAssignments>,
    #[serde(default)]
    pub open_positions: Option<i64>,
    /// Decimal-as-string (or number) — `None` until client-scoped fill
    /// enrichment lands server-side.
    #[serde(default)]
    pub unrealized_pnl_usd: Option<serde_json::Value>,
    #[serde(default)]
    pub last_activity: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// The endpoint's envelope: `{"clients": [...]}`.
#[derive(Debug, Deserialize)]
struct GatewayClientList {
    clients: Vec<GatewayClient>,
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("reqwest client")
}

/// Decode the list response: the live contract is an object envelope
/// (`{"clients": [...]}`); a bare array is tolerated for forward/
/// backward compat.
fn decode_gateway_clients(body: &str) -> Result<Vec<GatewayClient>, String> {
    if let Ok(list) = serde_json::from_str::<GatewayClientList>(body) {
        return Ok(list.clients);
    }
    serde_json::from_str::<Vec<GatewayClient>>(body)
        .map_err(|e| format!("GET /api/trading/clients decode: {e}"))
}

/// Fetch the gateway's client rows. `Ok(None)` when the endpoint
/// doesn't exist yet (404/405) — callers degrade to the local view.
async fn fetch_gateway_clients(
    auth: &crate::sol::gateway::GatewayAuth,
) -> Result<Option<Vec<GatewayClient>>, String> {
    let url = format!("{}/api/trading/clients", auth.base);
    let resp = http()
        .get(&url)
        .bearer_auth(&auth.token)
        .send()
        .await
        .map_err(|e| format!("GET /api/trading/clients: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 404 || status.as_u16() == 405 {
        return Ok(None);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GET /api/trading/clients: {status}: {body}"));
    }
    let body = resp.text().await.map_err(|e| e.to_string())?;
    decode_gateway_clients(&body).map(Some)
}

/// Register a local wallet server-side (drift repair). Best-effort.
/// A 409 `wallet_already_registered` from our own account is fine —
/// the next list fetch matches it by wallet; a foreign-account 409
/// surfaces as the error string.
/// Sentinel error: the gateway refused the auto-register because the
/// user deliberately removed this wallet from the account.
const REMOVED_TOMBSTONE: &str = "removed from your account";

async fn register_gateway_client(
    auth: &crate::sol::gateway::GatewayAuth,
    entry: &WalletEntry,
    revive: bool,
) -> Result<(), String> {
    let url = format!("{}/api/trading/clients", auth.base);
    let resp = http()
        .post(&url)
        .bearer_auth(&auth.token)
        .json(&serde_json::json!({
            "chain": gw_chain(entry.chain),
            "wallet_address": entry.address,
            "label": entry.label,
            "revive": revive,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if status.as_u16() == 409 {
        // Tombstone: the user removed this wallet from the account on
        // some device. The polling loop must not resurrect it; only an
        // explicit add/import on this device (revive) may.
        let body = resp.text().await.unwrap_or_default();
        if body.contains("client_removed") {
            return Err(REMOVED_TOMBSTONE.into());
        }
        // Already registered otherwise: by us (label drift only) or by
        // another account (global wallet uniqueness). Re-fetch and check.
        if let Ok(Some(rows)) = fetch_gateway_clients(auth).await {
            if rows.iter().any(|r| {
                r.wallet
                    .as_deref()
                    .is_some_and(|w| w.eq_ignore_ascii_case(&entry.address))
            }) {
                return Ok(());
            }
        }
        return Err("wallet already registered to another account".into());
    }
    if !status.is_success() {
        return Err(format!("{status}"));
    }
    Ok(())
}

/// Push a pause toggle server-side. Best-effort.
async fn pause_gateway_client(
    auth: &crate::sol::gateway::GatewayAuth,
    gateway_id: &str,
    paused: bool,
) -> Result<(), String> {
    let url = format!("{}/api/trading/clients/{gateway_id}/pause", auth.base);
    let resp = http()
        .post(&url)
        .bearer_auth(&auth.token)
        .json(&serde_json::json!({ "paused": paused }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("{}", resp.status()));
    }
    Ok(())
}

// ─── IPC: clients_* ─────────────────────────────────────────────────

/// Merged local + gateway view of one client. Addresses + statuses
/// only — never key material.
#[derive(Debug, Serialize)]
pub struct ClientInfo {
    /// Local vault wallet id, or `gw-<id>` for server-only rows.
    pub id: String,
    pub chain: String,
    pub address: String,
    pub label: Option<String>,
    /// Local per-client pause flag (gateway pause is in `gateway`).
    pub paused: bool,
    pub primary: bool,
    pub unlocked: bool,
    /// `executor | standby | locked | remote` + engine states
    /// (`ready`, `connecting`, …) for the executor.
    pub runtime_state: String,
    pub runtime_detail: Option<String>,
    /// Drift between local vault + gateway registry, when detectable.
    pub drift: Option<String>,
    /// The gateway's row, verbatim-ish. `None` while the endpoint
    /// doesn't exist or auth is missing.
    pub gateway: Option<GatewayClient>,
}

fn runtime_state_for(state: &AppState, entry: &WalletEntry) -> (String, Option<String>, bool) {
    let clients = match state.clients.lock() {
        Ok(g) => g,
        Err(_) => return ("locked".into(), None, false),
    };
    let Some(handle) = clients.iter().find(|c| c.entry.id == entry.id) else {
        return ("locked".into(), None, false);
    };
    match entry.chain {
        WalletChain::Sol => {
            // Every unlocked Sol wallet runs its own engine inside the
            // dispatcher — per-wallet telemetry, primary shares the
            // device-level struct.
            let snap = handle
                .sol_runtime
                .as_ref()
                .map(|rt| rt.snapshot())
                .unwrap_or_else(|| state.sol_runtime.snapshot());
            (format!("executor:{}", snap.state), snap.error, true)
        }
        WalletChain::Hl => {
            let rt = handle
                .hl_runtime
                .as_ref()
                .cloned()
                .unwrap_or_else(|| state.hl_runtime.clone());
            let conn = rt.conn.lock().ok().and_then(|g| *g);
            let err = rt.error.lock().ok().and_then(|g| g.clone());
            if handle.hl_executor.load(Ordering::Relaxed) {
                let label = match conn {
                    Some(c) => format!("executor:{c:?}").to_lowercase(),
                    None => "executor:offline".into(),
                };
                (label, err, true)
            } else if handle.role == ClientRole::Primary {
                // Unpaired primary — no daemon yet.
                let label = match conn {
                    Some(c) => format!("executor:{c:?}").to_lowercase(),
                    None => "executor:offline".into(),
                };
                (label, err, true)
            } else {
                let label = match conn {
                    Some(ConnState::Ready) => "standby:registered".to_string(),
                    Some(c) => format!("standby:{c:?}").to_lowercase(),
                    None => "standby".into(),
                };
                (label, err, true)
            }
        }
    }
}

/// List all clients: vault wallets merged with the gateway registry.
/// Local wallets missing server-side are auto-registered (best-effort)
/// and flagged via `drift`.
#[tauri::command]
pub async fn clients_list(state: State<'_, AppState>) -> Result<Vec<ClientInfo>, String> {
    // Local view first — must work fully offline.
    let vault = open_vault()?;
    let local: Vec<WalletEntry> = vault
        .as_ref()
        .map(|v| v.wallets().to_vec())
        .unwrap_or_default();
    let primary_sol = vault
        .as_ref()
        .and_then(|v| v.primary(WalletChain::Sol).map(|w| w.id.clone()));
    let primary_hl = vault
        .as_ref()
        .and_then(|v| v.primary(WalletChain::Hl).map(|w| w.id.clone()));

    // Gateway view — every failure degrades to local-only.
    let gw_rows: Option<Vec<GatewayClient>> = match crate::sol::gateway::resolve_auth(&state).await
    {
        Ok(auth) => match fetch_gateway_clients(&auth).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "gateway clients fetch failed — local-only view");
                None
            }
        },
        Err(_) => None,
    };
    let auth = crate::sol::gateway::resolve_auth(&state).await.ok();

    let mut out = Vec::with_capacity(local.len());
    for entry in &local {
        let gw = gw_rows.as_ref().and_then(|rows| {
            rows.iter()
                .find(|r| {
                    r.wallet
                        .as_deref()
                        .is_some_and(|w| w.eq_ignore_ascii_case(&entry.address))
                })
                .cloned()
        });
        let mut drift = None;
        if gw_rows.is_some() && gw.is_none() {
            // Local wallet unknown server-side → register it.
            let revive = state
                .revive_ok
                .lock()
                .map(|g| g.contains(&entry.address.to_lowercase()))
                .unwrap_or(false);
            drift = Some(match &auth {
                Some(a) => match register_gateway_client(a, entry, revive).await {
                    Ok(()) => {
                        if revive {
                            if let Ok(mut g) = state.revive_ok.lock() {
                                g.remove(&entry.address.to_lowercase());
                            }
                        }
                        "registered server-side just now".to_string()
                    }
                    Err(e) => format!("not registered server-side (auto-register failed: {e})"),
                },
                None => "not registered server-side".to_string(),
            });
        }
        let (runtime_state, runtime_detail, unlocked) = runtime_state_for(&state, entry);
        out.push(ClientInfo {
            id: entry.id.clone(),
            chain: entry.chain.as_str().to_string(),
            address: entry.address.clone(),
            label: entry.label.clone(),
            paused: entry.paused,
            primary: Some(&entry.id) == primary_sol.as_ref()
                || Some(&entry.id) == primary_hl.as_ref(),
            unlocked,
            runtime_state,
            runtime_detail,
            drift,
            gateway: gw,
        });
    }

    // Server-only rows (configured on another device / not in this
    // vault) — surfaced so the user sees the whole account.
    if let Some(rows) = &gw_rows {
        for r in rows {
            let known = r
                .wallet
                .as_deref()
                .is_some_and(|w| local.iter().any(|e| e.address.eq_ignore_ascii_case(w)));
            if known {
                continue;
            }
            out.push(ClientInfo {
                id: format!("gw-{}", r.id),
                chain: r
                    .chain
                    .as_deref()
                    .map(local_chain_name)
                    .unwrap_or_else(|| "?".into()),
                address: r.wallet.clone().unwrap_or_default(),
                label: r.label.clone(),
                paused: r.paused.unwrap_or(false),
                primary: false,
                unlocked: false,
                runtime_state: "remote".into(),
                runtime_detail: Some("registered on the gateway but not in this vault".into()),
                drift: Some("no local key for this client".into()),
                gateway: Some(r.clone()),
            });
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
pub struct ClientAddReq {
    /// "sol" — fresh keypair. (HL agent keys are minted on
    /// hyperliquid.xyz; use `client_import`.)
    pub chain: String,
    pub label: Option<String>,
    /// Master password (verified against the vault verifier).
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct ClientAddResp {
    pub id: String,
    pub chain: String,
    pub address: String,
}

/// Generate a fresh wallet into the vault (vault-append — re-runnable,
/// no exists-error). Runtimes pick it up on the next unlock, or
/// immediately when the wizard follows up with `unlock_keystores`.
#[tauri::command]
pub fn client_add(req: ClientAddReq, state: State<'_, AppState>) -> Result<ClientAddResp, String> {
    if req.chain != "sol" {
        return Err("generate your HL API agent key on hyperliquid.xyz then use Import".into());
    }
    let mut vault = open_or_create_vault_migrated(&req.password)?;
    let kp = core::Keypair::new();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(kp.secret_bytes().as_slice());
    drop(kp);
    let entry = vault
        .add_sol(&seed, &req.password, req.label)
        .map_err(|e| e.to_string())?;
    {
        use zeroize::Zeroize as _;
        seed.zeroize();
    }
    mark_revive_ok(&state, &entry.address);
    Ok(ClientAddResp {
        id: entry.id,
        chain: "sol".into(),
        address: entry.address,
    })
}

/// A deliberate add/import on THIS device may revive a wallet the user
/// removed from the account — arm a one-shot revive for the next
/// auto-register (see `register_gateway_client`).
fn mark_revive_ok(state: &State<'_, AppState>, address: &str) {
    if let Ok(mut g) = state.revive_ok.lock() {
        g.insert(address.to_lowercase());
    }
}

#[derive(Debug, Deserialize)]
pub struct ClientImportReq {
    /// "sol" | "hl".
    pub chain: String,
    /// sol: base58 or hex, 32 or 64 bytes. hl: 32-byte hex
    /// (0x-prefix optional).
    pub secret: String,
    pub label: Option<String>,
    pub password: String,
}

/// Import a pasted private key into the vault (per chain, N times).
#[tauri::command]
pub fn client_import(
    req: ClientImportReq,
    state: State<'_, AppState>,
) -> Result<ClientAddResp, String> {
    let mut vault = open_or_create_vault_migrated(&req.password)?;
    match req.chain.as_str() {
        "sol" => {
            let seed = crate::commands::parse_sol_secret(&req.secret)?;
            let entry = vault
                .add_sol(&seed, &req.password, req.label)
                .map_err(|e| e.to_string())?;
            mark_revive_ok(&state, &entry.address);
            Ok(ClientAddResp {
                id: entry.id,
                chain: "sol".into(),
                address: entry.address,
            })
        }
        "hl" => {
            let entry = vault
                .add_hl(req.secret.trim(), &req.password, req.label)
                .map_err(|e| e.to_string())?;
            mark_revive_ok(&state, &entry.address);
            Ok(ClientAddResp {
                id: entry.id,
                chain: "hl".into(),
                address: entry.address,
            })
        }
        other => Err(format!("unknown chain {other:?} (sol | hl)")),
    }
}

/// Unlock + register + start the runtime for ONE wallet that was just
/// appended to the vault while the app is already unlocked. Without
/// this, "+ Client" produced a paired-but-permanently-idle wallet:
/// runtimes were only ever built by the full unlock, and every re-arm
/// path iterates the (stale) in-memory snapshot — the new wallet sat
/// dead until the next app lock/unlock cycle.
///
/// Semantics:
/// - already in the live snapshot → no-op (idempotent),
/// - app locked / pre-vault (empty snapshot) → full unlock path, which
///   builds the entire topology including this wallet,
/// - otherwise → decrypt EXACTLY this wallet with the master password,
///   push its handle into the live snapshot, re-derive the chain
///   primary (a first-of-chain wallet becomes primary, arming the
///   legacy mirrors / `:5829` slot), recompute pause gates, and start
///   just this wallet's runtime. Siblings keep running untouched —
///   pause / primary / pair flows all see the new handle immediately.
#[tauri::command]
pub fn client_activate(id: String, password: String, app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();

    // Locked app (or pre-vault install): the full unlock builds
    // everything, including the wallet that was just added.
    let snapshot_empty = state.clients.lock().map(|g| g.is_empty()).unwrap_or(true);
    if snapshot_empty {
        return crate::commands::unlock_with_password(&app, &password, core::KeystoreBackend::File);
    }

    if state
        .clients
        .lock()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|c| c.entry.id == id)
    {
        return Ok(()); // already live
    }

    let vault = open_vault()?.ok_or("no vault on this device")?;
    vault
        .verify_password(&password)
        .map_err(|e| e.to_string())?;
    let entry = vault
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("unknown client {id}"))?;

    // First wallet of its chain falls back to primary (vault::primary).
    let is_primary = vault.primary(entry.chain).is_some_and(|p| p.id == entry.id);
    let role = if is_primary {
        ClientRole::Primary
    } else {
        ClientRole::Standby
    };

    // Decrypt EXACTLY this wallet — mirror of `unlock_clients`' per-entry
    // body, without rebuilding (and orphaning) the rest of the snapshot.
    let mut handle = ClientHandle {
        entry: entry.clone(),
        role,
        pause_gate: Arc::new(Mutex::new(entry.paused)),
        sol_seed: None,
        hl_secret_hex: None,
        hl_runtime: None,
        sol_runtime: None,
        hl_executor: Arc::new(AtomicBool::new(false)),
    };
    match entry.chain {
        WalletChain::Sol => {
            let kp = vault
                .unlock_sol(&entry.id, &password)
                .map_err(|e| format!("unlock {} ({}): {e}", entry.address, entry.id))?;
            let mut seed = [0u8; 32];
            seed.copy_from_slice(kp.secret_bytes().as_slice());
            handle.sol_seed = Some(seed);
            handle.sol_runtime = Some(if is_primary {
                state.sol_runtime.clone()
            } else {
                Arc::new(crate::sol::runtime::SolRuntimeInner::default())
            });
            drop(kp);
        }
        WalletChain::Hl => {
            let (secret_hex, _addr) = vault
                .unlock_hl(&entry.id, &password)
                .map_err(|e| format!("unlock {} ({}): {e}", entry.address, entry.id))?;
            handle.hl_secret_hex = Some(secret_hex);
            handle.hl_runtime = Some(if is_primary {
                state.hl_runtime.clone()
            } else {
                Arc::new(HlRuntime::default())
            });
        }
    }
    {
        let mut clients = state.clients.lock().map_err(|e| e.to_string())?;
        clients.push(handle);
    }

    // First-of-chain primary: arm the legacy single-wallet mirrors
    // (`:5829` slot / `hl_secret_hex`) exactly like unlock would. Only
    // safe to call here because there are no OTHER wallets of this
    // chain whose running runtimes a re-arm could orphan.
    if is_primary {
        match entry.chain {
            WalletChain::Sol => rearm_primary_sol(&state, &vault),
            WalletChain::Hl => rearm_primary_hl(&state, &vault),
        }
    }
    recompute_pause_gates(&state);

    // Start exactly THIS wallet's runtime; siblings stay untouched.
    match entry.chain {
        WalletChain::Sol => {
            // The dispatcher rebuilds its engine set from the live
            // snapshot — graceful replace (the old loop is signalled and
            // waited out), so the new wallet gets an engine immediately.
            crate::sol::runtime::spawn(&app);
        }
        WalletChain::Hl => {
            // Re-read the pushed handle's plumbing — the primary re-arm
            // above may have swapped runtime Arcs.
            let plan = state.clients.lock().ok().and_then(|clients| {
                clients.iter().find(|c| c.entry.id == id).map(|c| {
                    (
                        c.role,
                        c.hl_secret_hex.clone(),
                        c.pause_gate.clone(),
                        c.hl_runtime.clone(),
                        c.hl_executor.clone(),
                    )
                })
            });
            if let Some((role, Some(secret), pause_gate, hl_runtime, hl_executor)) = plan {
                start_hl_runtime(
                    &app,
                    &vault,
                    &entry,
                    role,
                    secret,
                    pause_gate,
                    hl_runtime,
                    hl_executor,
                );
            }
        }
    }
    Ok(())
}

/// Remove a wallet from the vault (keystore file kept as
/// `.removed.bak`). Stops its runtime + wipes its in-memory secret
/// first.
#[tauri::command]
pub fn client_remove(id: String, app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut vault = open_vault()?.ok_or("no vault on this device")?;
    let was_unlocked = {
        let mut clients = state.clients.lock().map_err(|e| e.to_string())?;
        if let Some(pos) = clients.iter().position(|c| c.entry.id == id) {
            let c = clients.remove(pos);
            if let Some(rt) = &c.hl_runtime {
                rt.daemon_running.store(false, Ordering::SeqCst);
                rt.set_conn(ConnState::Offline);
            }
            true
        } else {
            false
        }
    };
    let entry = vault.remove(&id).map_err(|e| e.to_string())?;
    if was_unlocked {
        // Primary fallback may have shifted — re-arm what's left.
        if entry.chain == WalletChain::Sol {
            state.sol_slot.clear();
            crate::sol::runtime::stop(&state);
            rearm_primary_sol(&state, &vault);
        }
        restart_runtimes(&app);
    }
    Ok(())
}

/// Delete a gateway-only client registration (a `gw-…` row surfaced by
/// `clients_list` — registered from another install of the app, no key
/// material in this vault). Rides `DELETE /api/trading/clients/{id}` on
/// the gateway; the id is the GATEWAY id (`gateway.id`), not the local
/// vault id. Server-side metadata only: keys on the device that created
/// the binding are untouched, and that install re-registers itself on
/// its next list refresh if it is still running.
#[tauri::command]
pub async fn client_gateway_deregister(
    state: State<'_, AppState>,
    gateway_id: String,
) -> Result<(), String> {
    let auth = crate::sol::gateway::resolve_auth(&state).await?;
    let url = format!("{}/api/trading/clients/{}", auth.base, gateway_id);
    let resp = http()
        .delete(&url)
        .bearer_auth(&auth.token)
        .send()
        .await
        .map_err(|e| format!("DELETE /api/trading/clients/{gateway_id}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("gateway {status}: {body}"));
    }
    Ok(())
}

/// Install the (possibly newly designated) primary Sol wallet into the
/// `:5829` slot + legacy state mirror, from the in-memory seeds.
fn rearm_primary_sol(state: &AppState, vault: &Vault) {
    use solana_sdk::signature::SeedDerivable;
    let primary_id = vault.primary(WalletChain::Sol).map(|w| w.id.clone());
    let mut installed = false;
    if let (Some(pid), Ok(mut clients)) = (primary_id, state.clients.lock()) {
        for c in clients.iter_mut() {
            let is_primary = c.entry.id == pid;
            if c.entry.chain == WalletChain::Sol {
                c.role = if is_primary {
                    ClientRole::Primary
                } else {
                    ClientRole::Standby
                };
                // The primary's engine telemetry IS the device-level
                // struct (legacy `sol_runtime_status`); a demoted
                // wallet gets its own.
                c.sol_runtime = Some(if is_primary {
                    state.sol_runtime.clone()
                } else {
                    Arc::new(crate::sol::runtime::SolRuntimeInner::default())
                });
            }
            if is_primary {
                if let Some(seed) = &c.sol_seed {
                    if let Ok(kp) = core::Keypair::from_seed(seed) {
                        state.sol_slot.install(kp);
                        installed = true;
                    }
                    if let Ok(mut g) = state.sol_seed.lock() {
                        *g = Some(*seed);
                    }
                }
            }
        }
    }
    if !installed {
        state.sol_slot.clear();
        if let Ok(mut g) = state.sol_seed.lock() {
            *g = None;
        }
    }
}

/// Re-point HL primary roles + the shared global runtime after a
/// designation change.
fn rearm_primary_hl(state: &AppState, vault: &Vault) {
    let primary_id = vault.primary(WalletChain::Hl).map(|w| w.id.clone());
    if let (Some(pid), Ok(mut clients)) = (primary_id, state.clients.lock()) {
        for c in clients.iter_mut() {
            if c.entry.chain != WalletChain::Hl {
                continue;
            }
            let is_primary = c.entry.id == pid;
            c.role = if is_primary {
                ClientRole::Primary
            } else {
                ClientRole::Standby
            };
            c.hl_runtime = Some(if is_primary {
                state.hl_runtime.clone()
            } else {
                Arc::new(HlRuntime::default())
            });
            if is_primary {
                if let Ok(mut g) = state.hl_secret_hex.lock() {
                    *g = c.hl_secret_hex.clone();
                }
            }
        }
    }
}

#[tauri::command]
pub fn client_label(id: String, label: Option<String>) -> Result<(), String> {
    let mut vault = open_vault()?.ok_or("no vault on this device")?;
    vault
        .set_label(&id, label.clone())
        .map_err(|e| e.to_string())
}

/// Toggle a single client's pause flag. Local-first (the vault flag is
/// the kill-switch); pushed to the gateway best-effort so the server
/// copy converges.
#[tauri::command]
pub async fn client_pause(id: String, paused: bool, app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut vault = open_vault()?.ok_or("no vault on this device")?;
    vault.set_paused(&id, paused).map_err(|e| e.to_string())?;
    let entry = vault
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("unknown client {id}"))?;

    // Mirror into the live handle + recompute gates. Both executors
    // read the per-client gate INLINE on the money path (HL: before
    // every poll; Sol: per dispatched event) — pausing one client
    // never stops or restarts a sibling's runtime.
    {
        let mut clients = state.clients.lock().map_err(|e| e.to_string())?;
        if let Some(c) = clients.iter_mut().find(|c| c.entry.id == id) {
            c.entry.paused = paused;
        }
    }
    recompute_pause_gates(&state);

    // Best-effort server sync.
    if let Ok(auth) = crate::sol::gateway::resolve_auth(&state).await {
        if let Ok(Some(rows)) = fetch_gateway_clients(&auth).await {
            if let Some(gw) = rows.iter().find(|r| {
                r.wallet
                    .as_deref()
                    .is_some_and(|w| w.eq_ignore_ascii_case(&entry.address))
            }) {
                if let Err(e) = pause_gateway_client(&auth, &gw.id, paused).await {
                    tracing::warn!(error = %e, "gateway client pause push failed (local pause holds)");
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct ClientRuntimeStatus {
    pub id: String,
    pub chain: String,
    pub role: Option<ClientRole>,
    pub unlocked: bool,
    pub paused: bool,
    pub runtime_state: String,
    pub runtime_detail: Option<String>,
    /// HL only: this wallet's balance telemetry (primary = global
    /// runtime, others = their own daemon/standby loop).
    pub hl_balance: Option<crate::commands::BalanceSnapshotDto>,
    /// Sol only: this wallet's OWN engine telemetry (per-wallet
    /// counters; the primary's doubles as `sol_runtime_status`).
    pub sol_status: Option<crate::sol::runtime::SolRuntimeStatus>,
}

#[tauri::command]
pub fn client_runtime_status(
    id: String,
    state: State<'_, AppState>,
) -> Result<ClientRuntimeStatus, String> {
    let vault = open_vault()?.ok_or("no vault on this device")?;
    let entry = vault
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("unknown client {id}"))?;
    let (runtime_state, runtime_detail, unlocked) = runtime_state_for(&state, &entry);
    let (role, hl_balance, sol_status) = state
        .clients
        .lock()
        .ok()
        .and_then(|clients| {
            clients.iter().find(|c| c.entry.id == id).map(|c| {
                let bal = c
                    .hl_runtime
                    .as_ref()
                    .and_then(|rt| rt.balance.lock().ok().map(|g| g.clone().into()));
                let sol = c.sol_runtime.as_ref().map(|rt| rt.snapshot());
                (Some(c.role), bal, sol)
            })
        })
        .unwrap_or((None, None, None));
    Ok(ClientRuntimeStatus {
        id,
        chain: entry.chain.as_str().to_string(),
        role,
        unlocked,
        paused: entry.paused,
        runtime_state,
        runtime_detail,
        hl_balance,
        sol_status,
    })
}

/// Designate a wallet as its chain's primary executor. Restarts the
/// affected runtimes (the old executor stops; the new one takes over).
#[tauri::command]
pub fn client_set_primary(id: String, app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let mut vault = open_vault()?.ok_or("no vault on this device")?;
    let chain = vault
        .get(&id)
        .map(|w| w.chain)
        .ok_or_else(|| format!("unknown client {id}"))?;
    vault.set_primary(&id).map_err(|e| e.to_string())?;
    // Stop FIRST — `rearm_primary_hl` swaps standby runtime Arcs, and a
    // stop after the swap would miss the old Arcs' loops entirely.
    stop_runtimes(&state);
    match chain {
        WalletChain::Sol => rearm_primary_sol(&state, &vault),
        WalletChain::Hl => rearm_primary_hl(&state, &vault),
    }
    restart_runtimes(&app);
    Ok(())
}

/// Export one wallet's ENCRYPTED keystore envelope to a user-chosen
/// path. Never plaintext key material.
#[tauri::command]
pub fn client_export_keystore(id: String, dest: String) -> Result<String, String> {
    let vault = open_vault()?.ok_or("no vault on this device")?;
    let json = vault.export_keystore_json(&id).map_err(|e| e.to_string())?;
    let dest_path = std::path::PathBuf::from(dest);
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&dest_path, json.as_bytes()).map_err(|e| e.to_string())?;
    Ok(dest_path.to_string_lossy().into_owned())
}

// ─── IPC: per-client gateway config pass-throughs ───────────────────
//
// All of these take the GATEWAY client id (`ClientInfo.gateway.id`) —
// the server is the source of truth for budgets, the HL single-active-
// config slot, and Sol preset assignments. 404/405 degrade gracefully
// ("endpoint not on this gateway yet") instead of panicking the UI.

const GW_TOO_OLD: &str = "this gateway does not support per-client management yet";

async fn gw_send(
    auth: &crate::sol::gateway::GatewayAuth,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<Option<String>, String> {
    let url = format!("{}{}", auth.base, path);
    let mut req = http()
        .request(method.clone(), &url)
        .bearer_auth(&auth.token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("{method} {path}: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 404 || status.as_u16() == 405 {
        // Distinguish "endpoint missing" from "row missing" is not
        // possible without the body — surface the body's error code
        // when it parses, else the capability message.
        let text = resp.text().await.unwrap_or_default();
        if text.contains("not_found") {
            return Err(format!("{method} {path}: not found"));
        }
        return Err(GW_TOO_OLD.into());
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        // Machine-readable gateway errors ({"error": "..."}). Pass the
        // body through — the UI renders the reason.
        return Err(format!("gateway {status}: {text}"));
    }
    Ok(resp.text().await.ok())
}

/// Budget update — any subset; `clear_*` flags null a field.
/// Mirrors `PUT /api/trading/clients/{id}/budget`.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ClientBudgetReq {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_budget_usd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_position_usd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_size_usd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_budget_lamports: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_trade_lamports: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_session_budget_usd: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_max_position_usd: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_default_size_usd: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_session_budget_lamports: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_per_trade_lamports: bool,
}

/// Set a client's server-side budget (the gateway enforces it on every
/// dispatch/intent). The UI reloads `clients_list` after.
#[tauri::command]
pub async fn client_budget_set(
    gateway_id: String,
    req: ClientBudgetReq,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = crate::sol::gateway::resolve_auth(&state).await?;
    let body = serde_json::to_value(&req).map_err(|e| e.to_string())?;
    gw_send(
        &auth,
        reqwest::Method::PUT,
        &format!("/api/trading/clients/{gateway_id}/budget"),
        Some(body),
    )
    .await
    .map(|_| ())
}

/// Replace an HL client's single active config (caller XOR copytrade) —
/// atomic server-side. `PUT /api/trading/clients/{id}/active-config`.
#[tauri::command]
pub async fn client_active_config_set(
    gateway_id: String,
    config_type: String,
    ref_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = crate::sol::gateway::resolve_auth(&state).await?;
    gw_send(
        &auth,
        reqwest::Method::PUT,
        &format!("/api/trading/clients/{gateway_id}/active-config"),
        Some(serde_json::json!({ "type": config_type, "ref_id": ref_id })),
    )
    .await
    .map(|_| ())
}

/// Clear an HL client's active config (disables the declarative row AND
/// the bound legacy execution rows).
#[tauri::command]
pub async fn client_active_config_clear(
    gateway_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = crate::sol::gateway::resolve_auth(&state).await?;
    gw_send(
        &auth,
        reqwest::Method::DELETE,
        &format!("/api/trading/clients/{gateway_id}/active-config"),
        None,
    )
    .await
    .map(|_| ())
}

/// One Sol preset assignment row, name-resolved for the UI.
#[derive(Debug, Serialize)]
pub struct ClientPresetDto {
    pub preset_id: String,
    pub name: String,
    pub enabled: bool,
    pub buy_size_lamports_override: Option<i64>,
    /// Raw `LegSpec[]` ladder override (display-only on this side).
    pub ladder_override: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GwPresetAssignment {
    preset_id: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    buy_size_lamports_override: Option<i64>,
    #[serde(default)]
    ladder_override: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GwPresetName {
    id: String,
    name: String,
}

/// List a Sol client's preset assignments
/// (`GET /api/trading/clients/{id}/presets`), preset names resolved
/// best-effort via `/api/alpha/presets`.
#[tauri::command]
pub async fn client_presets_list(
    gateway_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<ClientPresetDto>, String> {
    let auth = crate::sol::gateway::resolve_auth(&state).await?;
    let body = gw_send(
        &auth,
        reqwest::Method::GET,
        &format!("/api/trading/clients/{gateway_id}/presets"),
        None,
    )
    .await?
    .unwrap_or_default();
    let rows: Vec<GwPresetAssignment> =
        serde_json::from_str(&body).map_err(|e| format!("presets decode: {e}"))?;
    let names: std::collections::HashMap<String, String> =
        match gw_send(&auth, reqwest::Method::GET, "/api/alpha/presets", None).await {
            Ok(Some(b)) => serde_json::from_str::<Vec<GwPresetName>>(&b)
                .map(|v| v.into_iter().map(|p| (p.id, p.name)).collect())
                .unwrap_or_default(),
            _ => Default::default(),
        };
    Ok(rows
        .into_iter()
        .map(|r| {
            let name = names
                .get(&r.preset_id)
                .cloned()
                .unwrap_or_else(|| format!("preset {}…", &r.preset_id[..8.min(r.preset_id.len())]));
            ClientPresetDto {
                preset_id: r.preset_id,
                name,
                enabled: r.enabled,
                buy_size_lamports_override: r.buy_size_lamports_override,
                ladder_override: r.ladder_override,
            }
        })
        .collect())
}

/// Upsert a preset assignment on a Sol client
/// (`PUT /api/trading/clients/{id}/presets/{preset_id}`).
#[tauri::command]
pub async fn client_preset_assign(
    gateway_id: String,
    preset_id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = crate::sol::gateway::resolve_auth(&state).await?;
    gw_send(
        &auth,
        reqwest::Method::PUT,
        &format!("/api/trading/clients/{gateway_id}/presets/{preset_id}"),
        Some(serde_json::json!({ "enabled": enabled })),
    )
    .await
    .map(|_| ())
}

/// Remove a preset assignment
/// (`DELETE /api/trading/clients/{id}/presets/{preset_id}`).
#[tauri::command]
pub async fn client_preset_unassign(
    gateway_id: String,
    preset_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = crate::sol::gateway::resolve_auth(&state).await?;
    gw_send(
        &auth,
        reqwest::Method::DELETE,
        &format!("/api/trading/clients/{gateway_id}/presets/{preset_id}"),
        None,
    )
    .await
    .map(|_| ())
}

/// One Sol copy config bound to a client (leader resolved best-effort).
#[derive(Debug, Serialize)]
pub struct ClientCopyConfigDto {
    pub id: String,
    pub leader: String,
    pub label: String,
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
struct GwClientCopyConfig {
    id: String,
    tracked_wallet_id: String,
    #[serde(default)]
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct GwTrackedWalletLite {
    id: String,
    address: String,
    #[serde(default)]
    alias: Option<String>,
}

/// Sol copy configs assigned to THIS client
/// (`GET /api/trading/copy/configs?client_id=…`).
#[tauri::command]
pub async fn client_copy_configs(
    gateway_id: String,
    state: State<'_, AppState>,
) -> Result<Vec<ClientCopyConfigDto>, String> {
    let auth = crate::sol::gateway::resolve_auth(&state).await?;
    let body = gw_send(
        &auth,
        reqwest::Method::GET,
        &format!("/api/trading/copy/configs?client_id={gateway_id}"),
        None,
    )
    .await?
    .unwrap_or_default();
    let rows: Vec<GwClientCopyConfig> =
        serde_json::from_str(&body).map_err(|e| format!("copy configs decode: {e}"))?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let wallets: Vec<GwTrackedWalletLite> = match gw_send(
        &auth,
        reqwest::Method::GET,
        "/api/wallet-tracker/wallets",
        None,
    )
    .await
    {
        Ok(Some(b)) => serde_json::from_str(&b).unwrap_or_default(),
        _ => Vec::new(),
    };
    Ok(rows
        .into_iter()
        .map(|c| {
            let w = wallets.iter().find(|w| w.id == c.tracked_wallet_id);
            let leader = w
                .map(|w| w.address.clone())
                .unwrap_or_else(|| c.tracked_wallet_id.clone());
            let label = w
                .and_then(|w| w.alias.clone())
                .filter(|a| !a.trim().is_empty())
                .unwrap_or_else(|| {
                    if leader.len() > 10 {
                        format!("{}…{}", &leader[..4], &leader[leader.len() - 4..])
                    } else {
                        leader.clone()
                    }
                });
            ClientCopyConfigDto {
                id: c.id,
                leader,
                label,
                enabled: c.enabled,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    fn entry(id: &str, chain: WalletChain, address: &str, paused: bool) -> WalletEntry {
        WalletEntry {
            id: id.into(),
            chain,
            address: address.into(),
            label: None,
            created_at: chrono::Utc::now(),
            file: format!("{id}.json"),
            paused,
        }
    }

    fn handle(e: WalletEntry, role: ClientRole) -> ClientHandle {
        ClientHandle {
            pause_gate: Arc::new(Mutex::new(e.paused)),
            entry: e,
            role,
            sol_seed: None,
            hl_secret_hex: None,
            hl_runtime: Some(Arc::new(HlRuntime::default())),
            sol_runtime: None,
            hl_executor: Arc::new(AtomicBool::new(true)),
        }
    }

    fn gate(state: &AppState, id: &str) -> bool {
        let clients = state.clients.lock().unwrap();
        let c = clients.iter().find(|c| c.entry.id == id).unwrap();
        let g = *c.pause_gate.lock().unwrap();
        g
    }

    /// Safety test (c), HL half: pausing client A flips ONLY A's
    /// effective gate — sibling daemons (which read their own gate
    /// inline on the money path) are untouched. The global kill-switch
    /// still gates everyone.
    #[test]
    fn per_client_pause_isolates_siblings() {
        let state = AppState::default();
        {
            let mut clients = state.clients.lock().unwrap();
            clients.push(handle(
                entry("a", WalletChain::Hl, "0xaaa", false),
                ClientRole::Primary,
            ));
            clients.push(handle(
                entry("b", WalletChain::Hl, "0xbbb", false),
                ClientRole::Standby,
            ));
        }
        recompute_pause_gates(&state);
        assert!(!gate(&state, "a"));
        assert!(!gate(&state, "b"));

        // Pause A only.
        {
            let mut clients = state.clients.lock().unwrap();
            clients
                .iter_mut()
                .find(|c| c.entry.id == "a")
                .unwrap()
                .entry
                .paused = true;
        }
        recompute_pause_gates(&state);
        assert!(gate(&state, "a"), "paused client's gate must close");
        assert!(!gate(&state, "b"), "sibling's gate must stay open");

        // Resume A, flip the global kill-switch → both gated.
        {
            let mut clients = state.clients.lock().unwrap();
            clients
                .iter_mut()
                .find(|c| c.entry.id == "a")
                .unwrap()
                .entry
                .paused = false;
            *state.paused.lock().unwrap() = true;
        }
        recompute_pause_gates(&state);
        assert!(gate(&state, "a"));
        assert!(gate(&state, "b"));
    }

    // ── gateway contract (stub #6 verification, pinned) ────────────

    #[test]
    fn gateway_clients_decode_envelope_and_bare_array() {
        // The live contract (crates/modules/trading/src/api/clients.rs
        // `ClientListResponse`): an object envelope. Field names pinned
        // against `ClientSummary`.
        let body = serde_json::json!({
            "clients": [{
                "id": "0c0e0000-0000-0000-0000-000000000001",
                "chain": "hyperliquid",
                "wallet": "0xabc",
                "label": "Client 1",
                "paused": false,
                "budget": {
                    "session_budget_usd": "1000",
                    "max_position_usd": null,
                    "default_size_usd": "100",
                    "session_budget_lamports": null,
                    "per_trade_lamports": null
                },
                "active_config": { "type": "caller", "ref_id": "user:123", "since": "2026-06-24T12:00:00Z" },
                "assignments": { "presets": 2, "copytrade": 1 },
                "open_positions": null,
                "unrealized_pnl_usd": null,
                "last_activity": "2026-06-24T11:58:03Z",
                "created_at": "2026-06-10T09:00:00Z"
            }]
        })
        .to_string();
        let rows = decode_gateway_clients(&body).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.chain.as_deref(), Some("hyperliquid"));
        assert_eq!(r.wallet.as_deref(), Some("0xabc"));
        let b = r.budget.as_ref().unwrap();
        assert_eq!(
            b.session_budget_usd.as_ref().and_then(|v| v.as_str()),
            Some("1000")
        );
        let ac = r.active_config.as_ref().unwrap();
        assert_eq!(ac.r#type, "caller");
        assert_eq!(ac.ref_id, "user:123");
        let asg = r.assignments.as_ref().unwrap();
        assert_eq!((asg.presets, asg.copytrade), (2, 1));

        // Bare-array tolerance + minimal row (only id) both decode.
        let rows = decode_gateway_clients(r#"[{"id":"x"}]"#).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].budget.is_none());
    }

    #[test]
    fn chain_names_map_between_gateway_and_vault() {
        assert_eq!(gw_chain(WalletChain::Sol), "solana");
        assert_eq!(gw_chain(WalletChain::Hl), "hyperliquid");
        assert_eq!(local_chain_name("solana"), "sol");
        assert_eq!(local_chain_name("hyperliquid"), "hl");
        assert_eq!(local_chain_name("future-chain"), "future-chain");
    }

    #[test]
    fn budget_req_serializes_subset_only() {
        // The PUT /budget contract: omitted = unchanged. `None` fields
        // and false `clear_*` flags must vanish from the wire.
        let req = ClientBudgetReq {
            max_position_usd: Some("500".into()),
            clear_default_size_usd: true,
            ..Default::default()
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "max_position_usd": "500", "clear_default_size_usd": true })
        );
    }
}
