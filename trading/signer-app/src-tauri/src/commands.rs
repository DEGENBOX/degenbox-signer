//! Tauri IPC commands exposed to the React frontend.
//!
//! The frontend can ONLY call functions enumerated here — Tauri's
//! `invoke_handler` macro generates a hand-written switch, so a typo
//! on the JS side returns an error rather than executing arbitrary
//! code. Every command is `#[tauri::command]` + returns
//! `Result<T, String>` so errors flow as strings to the frontend's
//! `try { invoke(...) } catch { … }`.
//!
//! Private keys NEVER cross this boundary. The frontend gets:
//!
//! - pubkeys / addresses (safe, public)
//! - status enums (green/amber/red)
//! - recent-signs metadata (no secrets)
//!
//! Anything that would expose secret bytes (e.g. exporting a
//! generated wallet's seed phrase) goes through a dedicated command
//! that produces a one-shot disposable string and then wipes it.

use crate::hl::config::{HlConfig, NetworkChoice};
use crate::hl::daemon::{self, DaemonOpts};
use crate::hl::runtime::{BalanceSnapshot, ConnState, TotpPrompt};
use crate::hl::server::{
    RedeemRegistrationReq, RegisterReq as HlRegisterReq, ServerClient, ServerError,
};
use crate::state::{AppState, RecentSign, SignerHealth};
use degenbox_signer_core as core;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tauri::{Manager, State};

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub health: SignerHealth,
    pub paused: bool,
    pub hl_unlocked: bool,
    pub sol_unlocked: bool,
    pub hl_address: Option<String>,
    pub sol_pubkey: Option<String>,
    pub version: &'static str,
}

#[tauri::command]
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Move the on-disk keystore directory aside so the app re-onboards on the next
/// launch. Backs the "forgot passphrase / start fresh" action on the Unlock
/// screen: a locked vault whose passphrase is lost is otherwise a dead end (no
/// way back to the setup wizard without a terminal). We RENAME — never delete —
/// to a `.bak` sibling so any real keys stay recoverable from the backup.
/// Returns the backup path (empty string if there was nothing to move).
#[tauri::command]
pub fn reset_keystore() -> Result<String, String> {
    let dir = core::default_dir().map_err(|e| e.to_string())?;
    if !dir.exists() {
        return Ok(String::new());
    }
    let parent = dir
        .parent()
        .ok_or_else(|| "keystore dir has no parent".to_string())?;
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("degenbox");
    let mut bak = parent.join(format!("{name}.bak"));
    let mut n = 1u32;
    while bak.exists() {
        n += 1;
        bak = parent.join(format!("{name}.bak{n}"));
    }
    std::fs::rename(&dir, &bak).map_err(|e| e.to_string())?;
    Ok(bak.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn signer_status(state: State<'_, AppState>) -> Result<StatusReport, String> {
    // Vault primaries first; legacy single keystores pre-migration.
    let hl_address = crate::clients::primary_hl_address();
    let sol_pubkey = crate::clients::primary_sol_pubkey();
    let paused = *state.paused.lock().map_err(|e| e.to_string())?;
    let health = *state.health.lock().map_err(|e| e.to_string())?;
    let hl_unlocked = state
        .hl_secret_hex
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false);
    let sol_unlocked = state.sol_seed.lock().map(|g| g.is_some()).unwrap_or(false);
    Ok(StatusReport {
        health,
        paused,
        hl_unlocked,
        sol_unlocked,
        hl_address,
        sol_pubkey,
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[tauri::command]
pub fn set_paused(paused: bool, app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    {
        let mut g = state.paused.lock().map_err(|e| e.to_string())?;
        *g = paused;
    }
    // Persist — a paused signer must STAY paused across a relaunch,
    // especially now that the keychain auto-unlock brings the daemons
    // online without any user interaction.
    persist_paused(paused);
    // Per-client gates derive from (global || local) — recompute so the
    // HL daemons + the Sol dispatcher see the change on their next
    // poll/event.
    crate::clients::recompute_pause_gates(&state);
    // The global kill-switch additionally stops/starts the whole Sol
    // dispatcher (matching the button's "both chains" promise); the
    // per-client pause is handled inline by the dispatcher's gates.
    if paused {
        crate::sol::runtime::stop(&state);
    } else {
        crate::sol::runtime::spawn(&app);
    }
    Ok(())
}

fn paused_marker_path() -> Option<std::path::PathBuf> {
    core::default_dir().ok().map(|d| d.join("paused"))
}

/// Marker-file persistence for the kill-switch: file present = paused.
/// Best-effort — a failed write only costs persistence, never the live
/// toggle.
fn persist_paused(paused: bool) {
    let Some(path) = paused_marker_path() else {
        return;
    };
    let res = if paused {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, b"paused\n")
    } else {
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    };
    if let Err(e) = res {
        tracing::warn!(error = %e, "could not persist the pause state");
    }
}

/// Boot: restore a persisted pause BEFORE the keychain auto-unlock can
/// spawn any daemon, so "paused" survives a relaunch.
pub fn restore_paused(state: &AppState) {
    let paused = paused_marker_path().is_some_and(|p| p.exists());
    if paused {
        if let Ok(mut g) = state.paused.lock() {
            *g = true;
        }
        tracing::info!("restored persisted pause — signing stays paused until resumed");
    }
}

#[derive(Debug, Serialize)]
pub struct OnboardingState {
    pub needs_onboarding: bool,
    pub has_hl_keystore: bool,
    pub has_sol_keystore: bool,
    pub backend: Option<core::KeystoreBackend>,
}

#[tauri::command]
pub fn onboarding_state() -> Result<OnboardingState, String> {
    let hl = crate::clients::has_any_wallet(core::WalletChain::Hl);
    let sol = crate::clients::has_any_wallet(core::WalletChain::Sol);
    Ok(OnboardingState {
        needs_onboarding: !hl && !sol,
        has_hl_keystore: hl,
        has_sol_keystore: sol,
        // Backend choice persistence is a Settings.json TODO — for
        // now the frontend remembers it in tauri-store on the JS
        // side. Returning `None` triggers the picker on first run.
        backend: None,
    })
}

#[derive(Debug, Serialize)]
pub struct GenerateSolanaResult {
    pub pubkey: String,
}

/// Generate a fresh Solana wallet into the VAULT (vault-append — the
/// wizard is re-runnable; each run adds another wallet). The first
/// wallet of the chain becomes the primary by default.
#[tauri::command]
pub fn generate_solana_wallet(
    password: String,
    state: tauri::State<'_, crate::state::AppState>,
) -> Result<GenerateSolanaResult, String> {
    let resp = crate::clients::client_add(
        crate::clients::ClientAddReq {
            chain: "sol".into(),
            label: None,
            password,
        },
        state,
    )?;
    Ok(GenerateSolanaResult {
        pubkey: resp.address,
    })
}

#[derive(Debug, Deserialize)]
pub struct ImportSolanaReq {
    /// 32-byte seed as either base58 or hex.
    pub secret: String,
    pub password: String,
}

/// Parse a pasted Solana secret: base58 or hex, 32-byte seed or
/// 64-byte expanded (seed || pubkey).
pub fn parse_sol_secret(secret: &str) -> Result<[u8; 32], String> {
    let bytes = if let Ok(b) = bs58::decode(secret.trim()).into_vec() {
        b
    } else {
        hex::decode(secret.trim().trim_start_matches("0x"))
            .map_err(|e| format!("secret must be base58 or hex: {e}"))?
    };
    match bytes.len() {
        32 => Ok(bytes.as_slice().try_into().unwrap()),
        64 => Ok(bytes[..32].try_into().unwrap()),
        n => Err(format!("secret must be 32 or 64 bytes, got {n}")),
    }
}

#[tauri::command]
pub fn import_solana_wallet(
    req: ImportSolanaReq,
    state: tauri::State<'_, crate::state::AppState>,
) -> Result<GenerateSolanaResult, String> {
    let resp = crate::clients::client_import(
        crate::clients::ClientImportReq {
            chain: "sol".into(),
            secret: req.secret,
            label: None,
            password: req.password,
        },
        state,
    )?;
    Ok(GenerateSolanaResult {
        pubkey: resp.address,
    })
}

#[derive(Debug, Serialize)]
pub struct GenerateHlResult {
    pub address: String,
}

#[derive(Debug, Deserialize)]
pub struct ImportHlReq {
    /// 32-byte secp256k1 secret as hex (0x-prefix optional).
    pub private_key_hex: String,
    pub password: String,
}

#[tauri::command]
pub fn generate_hl_keystore() -> Result<GenerateHlResult, String> {
    // HL agent keys are minted on hyperliquid.xyz (the user pastes
    // their generated API agent key). Pure-random generation here
    // would produce an address that's not registered with HL and
    // therefore can't sign /exchange. We deliberately refuse and
    // surface a friendly hint to the frontend.
    Err("Generate your HL API agent key on hyperliquid.xyz then use Import.".into())
}

/// Import an HL agent key into the VAULT (vault-append, re-runnable —
/// each run adds another HL wallet).
#[tauri::command]
pub fn import_hl_keystore(
    req: ImportHlReq,
    state: tauri::State<'_, crate::state::AppState>,
) -> Result<GenerateHlResult, String> {
    let resp = crate::clients::client_import(
        crate::clients::ClientImportReq {
            chain: "hl".into(),
            secret: req.private_key_hex,
            label: None,
            password: req.password,
        },
        state,
    )?;
    Ok(GenerateHlResult {
        address: resp.address,
    })
}

#[derive(Debug, Deserialize)]
pub struct UnlockReq {
    pub password: String,
    pub backend: core::KeystoreBackend,
}

#[tauri::command]
pub fn unlock_keystores(req: UnlockReq, app: tauri::AppHandle) -> Result<(), String> {
    unlock_with_password(&app, &req.password, req.backend)
}

/// Shared unlock core — used by the `unlock_keystores` IPC command and
/// the boot-time keychain auto-unlock.
///
/// Master-password flow: open (or create) the VAULT, adopt any legacy
/// single-file keystores into it (originals kept as `.bak`), decrypt
/// EVERY wallet, arm the `:5829` slot with the primary Sol wallet,
/// optionally cache the passphrase, and bring all per-client runtimes
/// online (serialized topology — see `clients::start_runtimes`).
pub fn unlock_with_password(
    app: &tauri::AppHandle,
    password: &str,
    backend: core::KeystoreBackend,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let vault_dir = crate::clients::vault_dir()?;
    let hl_path = core::hl_keystore_path().map_err(|e| e.to_string())?;
    let sol_path = core::sol_keystore_path().map_err(|e| e.to_string())?;

    let have_anything = core::Vault::exists(&vault_dir) || hl_path.exists() || sol_path.exists();
    if !have_anything {
        // Nothing to unlock — keep the legacy no-op semantics.
        state.set_health(SignerHealth::Green);
        crate::sol::runtime::spawn(app);
        return Ok(());
    }

    // Vault-first: verify the password BEFORE any migration, then adopt
    // legacy keystores (non-destructive — `.bak` originals).
    let vault = crate::clients::open_or_create_vault_migrated(password)?;

    // Decrypt every wallet into per-client handles.
    crate::clients::unlock_clients(&state, &vault, password)?;

    // Mirror the primaries into the legacy single-wallet fields + the
    // `:5829` signer-protocol slot (backward-compat surfaces).
    {
        let clients = state.clients.lock().map_err(|e| e.to_string())?;
        let primary_sol = vault.primary(core::WalletChain::Sol).map(|w| w.id.clone());
        let primary_hl = vault.primary(core::WalletChain::Hl).map(|w| w.id.clone());
        for c in clients.iter() {
            if Some(&c.entry.id) == primary_sol.as_ref() {
                if let Some(seed) = &c.sol_seed {
                    use solana_sdk::signature::SeedDerivable;
                    if let Ok(kp) = core::Keypair::from_seed(seed) {
                        state.sol_slot.install(kp);
                    }
                    let mut g = state.sol_seed.lock().map_err(|e| e.to_string())?;
                    *g = Some(*seed);
                }
            }
            if Some(&c.entry.id) == primary_hl.as_ref() {
                let mut g = state.hl_secret_hex.lock().map_err(|e| e.to_string())?;
                g.clone_from(&c.hl_secret_hex);
            }
        }
    }

    // Cache passphrase if user opted into the OS keychain backend.
    if backend == core::KeystoreBackend::Keychain {
        if let Err(e) = core::os_keychain::store("primary", password) {
            tracing::warn!(error = %e, "OS keychain store failed — falling back to file-only");
        }
    }

    state.set_health(SignerHealth::Green);

    // Bring all per-client runtimes online: HL primary daemon, HL
    // standby loops, the Sol primary engine. Unpaired HL wallets stay
    // idle until `hl_pair`.
    crate::clients::recompute_pause_gates(&state);
    crate::clients::start_runtimes(app);
    Ok(())
}

/// Boot-time auto-unlock: if the user opted into the OS-keychain
/// backend on a previous unlock, the cached passphrase brings the
/// keystores online without a prompt. Silent no-op when there's no
/// cached entry, no keystore, or the passphrase no longer matches
/// (e.g. the keystore was replaced) — the app simply starts locked.
pub fn try_keychain_auto_unlock(app: &tauri::AppHandle) {
    let hl_exists = crate::clients::has_any_wallet(core::WalletChain::Hl);
    let sol_exists = crate::clients::has_any_wallet(core::WalletChain::Sol);
    if !hl_exists && !sol_exists {
        return;
    }
    let password = match core::os_keychain::load("primary") {
        Ok(p) => p,
        Err(core::OsKeychainError::NotFound) => return,
        Err(e) => {
            tracing::info!(error = %e, "keychain unavailable — starting locked");
            return;
        }
    };
    match unlock_with_password(app, &password, core::KeystoreBackend::Keychain) {
        Ok(()) => tracing::info!("keystores auto-unlocked from the OS keychain"),
        Err(e) => {
            tracing::warn!(error = %e, "keychain auto-unlock failed — starting locked")
        }
    }
}

/// Claim scope for the designated PRIMARY HL wallet: wallet-scoped when
/// the master account is known, and additionally the owner of legacy
/// UNSTAMPED rows (old gateways / pre-stamp backlog) so those execute
/// exactly once — on the wallet that always owned them.
pub fn primary_claim_scope(cfg: &HlConfig) -> crate::hl::daemon::CoreClaimScope {
    match cfg.account_address.as_deref() {
        Some(master) => crate::hl::daemon::CoreClaimScope::Scoped {
            wallet: master.to_string(),
            allow_unstamped: true,
        },
        None => crate::hl::daemon::CoreClaimScope::Unscoped,
    }
}

/// Spawn (or re-spawn) the background HL signing daemon for the
/// PRIMARY HL wallet. Back-compat wrapper around
/// [`spawn_hl_daemon_with`]: global telemetry + the primary client's
/// pause gate (global kill-switch when no client handle exists yet).
pub fn spawn_hl_daemon(app: &tauri::AppHandle, secret_hex: String, cfg: HlConfig) {
    let state = app.state::<AppState>();
    let pause = state
        .clients
        .lock()
        .ok()
        .and_then(|clients| {
            clients
                .iter()
                .find(|c| {
                    c.entry.chain == core::WalletChain::Hl
                        && c.role == crate::state::ClientRole::Primary
                })
                .map(|c| c.pause_gate.clone())
        })
        .unwrap_or_else(|| state.paused.clone());
    let runtime = state.hl_runtime.clone();
    // Same ledger the orchestrated spawn uses: per-wallet when this
    // primary is a vault wallet (seeded from the legacy global file),
    // else the global path (pre-migration installs).
    let executed = crate::clients::open_vault().ok().flatten().and_then(|v| {
        v.primary(core::WalletChain::Hl)
            .map(|e| crate::clients::hl_executed_path_seeded(&v, e))
    });
    let scope = primary_claim_scope(&cfg);
    spawn_hl_daemon_with(app, secret_hex, cfg, runtime, pause, executed, scope);
}

/// Spawn an HL signing daemon with explicit plumbing (per-client
/// telemetry, per-client pause gate, optional per-wallet
/// executed-marker ledger). Idempotent per runtime: the
/// `daemon_running` flag guards against a second poller racing the
/// first for the gateway's claim-on-read queue (which would
/// double-sign).
pub fn spawn_hl_daemon_with(
    app: &tauri::AppHandle,
    secret_hex: String,
    cfg: HlConfig,
    runtime: crate::hl::runtime::SharedHlRuntime,
    pause: std::sync::Arc<std::sync::Mutex<bool>>,
    executed_path: Option<std::path::PathBuf>,
    claim_scope: crate::hl::daemon::CoreClaimScope,
) {
    // CAS: only spawn if not already running.
    if runtime
        .daemon_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        tracing::info!("HL daemon already running — not spawning a second poller");
        return;
    }
    // We won the CAS — bump the run generation. Any stale loop that
    // missed the brief `daemon_running=false` window (a stop→respawn
    // cycle re-armed the flag before its next tick) sees the mismatch
    // and exits, so exactly ONE poller owns the claim queue.
    let my_generation = runtime.run_generation.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(agent_address) = cfg.agent_address.clone() else {
        runtime.daemon_running.store(false, Ordering::SeqCst);
        return;
    };
    let opts = DaemonOpts {
        config: cfg.clone(),
        secret_hex,
        agent_address,
        poll_interval: Duration::from_secs(cfg.poll_secs.max(1)),
        paper_mode: cfg.paper_mode,
        pause,
        runtime: runtime.clone(),
        app: app.clone(),
        executed_path,
        claim_scope,
    };
    let app2 = app.clone();
    // tauri::async_runtime, NOT tokio::spawn: callers include the sync
    // `unlock_keystores` IPC command, which runs on a webview thread
    // with no tokio context — a raw tokio::spawn aborts the whole app
    // there (SIGABRT at first unlock).
    tauri::async_runtime::spawn(async move {
        let rt = runtime;
        if let Err(e) = daemon::run(opts).await {
            tracing::error!(error = %e, "HL daemon exited with error");
            rt.set_conn(ConnState::Error);
            rt.set_error(Some(format!("daemon stopped: {e}")));
            app2.state::<AppState>().set_health(SignerHealth::Red);
        }
        // Loop exited (error or stop) — clear the running guard so a
        // subsequent unlock/pair can spawn a fresh one. ONLY if no newer
        // spawn superseded us: a stale generation must not relock the
        // successor's guard.
        if rt.run_generation.load(Ordering::SeqCst) == my_generation {
            rt.daemon_running.store(false, Ordering::SeqCst);
        }
    });
}

#[tauri::command]
pub fn lock_keystores(state: State<'_, AppState>) -> Result<(), String> {
    // An explicit lock must stick across a relaunch — drop the cached
    // keychain passphrase too, otherwise boot auto-unlock would undo it.
    match core::os_keychain::delete("primary") {
        Ok(()) | Err(core::OsKeychainError::NotFound) => {}
        Err(e) => tracing::warn!(error = %e, "could not clear cached keychain passphrase"),
    }
    if let Ok(mut g) = state.hl_secret_hex.lock() {
        *g = None;
    }
    if let Ok(mut g) = state.sol_seed.lock() {
        *g = None;
    }
    // Relock the :5829 daemon — key-requiring endpoints answer 503 again.
    state.sol_slot.clear();
    // Stop every per-client runtime (HL primary daemon, HL standby
    // loops, the Sol engine) — they re-spawn on the next unlock.
    crate::clients::stop_runtimes(&state);
    // Drop every decrypted per-client secret.
    if let Ok(mut clients) = state.clients.lock() {
        clients.clear();
    }
    state.set_health(SignerHealth::Red);
    Ok(())
}

#[tauri::command]
pub fn list_recent_signs(state: State<'_, AppState>) -> Result<Vec<RecentSign>, String> {
    let g = state.recent.lock().map_err(|e| e.to_string())?;
    Ok(g.clone())
}

#[derive(Debug, Deserialize)]
pub struct HlPairReq {
    pub server_url: String,
    /// One-shot onboarding token (32 hex chars → `redeem-registration`)
    /// OR a long-lived API token (anything else → bearer `register`).
    pub token: String,
    /// The user's HL MASTER wallet (0x…). REQUIRED for trade delivery:
    /// the gateway only hands out instructions to a heartbeat row that
    /// has `paired_with_account` set. Must NOT be the agent address.
    pub account_address: String,
    /// Optional 6-digit TOTP code, attached on the retried redeem after
    /// the gateway answers the first attempt with a 428 `totp_required`.
    #[serde(default)]
    pub totp_code: Option<String>,
    /// Vault wallet id to pair. Default = the primary HL wallet (the
    /// legacy single-wallet behaviour). Secondary wallets get their own
    /// per-wallet pairing config in the vault.
    #[serde(default)]
    pub client_id: Option<String>,
    /// Alternative wallet selector for callers that only know the agent
    /// address (the wizard right after an import). `client_id` wins.
    #[serde(default)]
    pub agent_address: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HlPairResp {
    pub user_id: String,
    pub agent_address: String,
    pub discord_handle: Option<String>,
    /// True when the gateway requires a TOTP code to complete pairing.
    /// The frontend prompts and re-calls `hl_pair` with `totp_code`.
    pub needs_totp: bool,
}

/// Pair this signer with the DegenBox gateway: registers the agent
/// address + master account so the gateway flips `signer/status` ready
/// and starts delivering instructions. Persists the minted JWT (or the
/// supplied bearer) + agent + master into the shared HL config, then
/// spawns the daemon (the HL keystore must already be unlocked).
///
/// Mirrors `hl-signer-desktop register`: validates the master address is
/// a 0x 20-byte hex that is NOT the agent address, prefers the one-shot
/// `redeem-registration` flow for 32-hex tokens, and handles the 428
/// TOTP retry.
#[tauri::command]
pub async fn hl_pair(
    req: HlPairReq,
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<HlPairResp, String> {
    // Resolve the wallet being paired: explicit client_id, else by
    // agent address, else the primary HL wallet (vault), else the
    // legacy single keystore.
    let vault = crate::clients::open_vault()?;
    let target_entry: Option<core::WalletEntry> = match (&vault, &req.client_id) {
        (Some(v), Some(id)) => Some(
            v.get(id)
                .cloned()
                .ok_or_else(|| format!("unknown client {id}"))?,
        ),
        (Some(v), None) => match &req.agent_address {
            Some(addr) => Some(
                v.wallets()
                    .iter()
                    .find(|w| {
                        w.chain == core::WalletChain::Hl && w.address.eq_ignore_ascii_case(addr)
                    })
                    .cloned()
                    .ok_or_else(|| format!("no vault wallet with agent address {addr}"))?,
            ),
            None => v.primary(core::WalletChain::Hl).cloned(),
        },
        (None, Some(_)) => return Err("no vault on this device — unlock once first".into()),
        (None, None) => None,
    };
    let agent_address = match &target_entry {
        Some(e) => {
            if e.chain != core::WalletChain::Hl {
                return Err("client is not a Hyperliquid wallet".into());
            }
            e.address.clone()
        }
        None => {
            let ks_path = core::hl_keystore_path().map_err(|e| e.to_string())?;
            if !ks_path.exists() {
                return Err("import your HL agent key first (Keys tab)".into());
            }
            core::hl_peek_address(&ks_path).map_err(|e| e.to_string())?
        }
    };
    let is_primary_target = match (&vault, &target_entry) {
        (Some(v), Some(e)) => v
            .primary(core::WalletChain::Hl)
            .is_some_and(|p| p.id == e.id),
        _ => true,
    };

    // Validate the master account: 0x + 20 bytes hex, and NOT the agent.
    let account = req.account_address.trim().to_ascii_lowercase();
    if !(account.starts_with("0x")
        && account.len() == 42
        && account[2..].chars().all(|c| c.is_ascii_hexdigit()))
    {
        return Err(format!(
            "master account must be a 0x-prefixed 20-byte hex address (got {account})"
        ));
    }
    if account.eq_ignore_ascii_case(&agent_address) {
        return Err(
            "master account must be your Hyperliquid MAIN wallet, NOT the agent address. \
             The agent is sandboxed and holds no positions."
                .into(),
        );
    }

    let mut cfg = match (&vault, &target_entry) {
        (Some(v), Some(e)) => crate::clients::hl_config_for(v, e, is_primary_target),
        _ => HlConfig::load_or_default(),
    };
    cfg.server_url = req.server_url.trim_end_matches('/').to_string();
    cfg.agent_address = Some(agent_address.clone());
    cfg.account_address = Some(account.clone());

    // Empty token → fall back to the Discord desktop login's JWT as the
    // bearer for the `register` path. One linked account pairs the HL
    // signer without a manually copied connect token.
    let mut raw_token = req.token.trim().to_string();
    if raw_token.is_empty() {
        raw_token = crate::auth::DesktopAuth::load_valid()
            .map(|a| a.token)
            .ok_or_else(|| {
                "no connect token — paste one from the DegenBox dashboard, or link your \
                 Discord account first (account menu, top right)"
                    .to_string()
            })?;
    }
    let is_registration_token =
        raw_token.len() == 32 && raw_token.chars().all(|c| c.is_ascii_hexdigit());

    let resp = if is_registration_token {
        let body = RedeemRegistrationReq {
            token: raw_token,
            agent_address: agent_address.clone(),
            client_version: Some(format!("degenbox-signer-app {}", env!("CARGO_PKG_VERSION"))),
            host_id: cfg.host_id.clone(),
            paired_with_account: Some(account.clone()),
            totp_code: req.totp_code.clone(),
        };
        match ServerClient::redeem_registration(&cfg.server_url, &body).await {
            Ok(r) => r,
            Err(ServerError::Status(status, b))
                if status.as_u16() == 428 && b.contains("totp_required") =>
            {
                // Signal the frontend to collect a TOTP code and re-call.
                return Ok(HlPairResp {
                    user_id: String::new(),
                    agent_address,
                    discord_handle: None,
                    needs_totp: true,
                });
            }
            Err(e) => return Err(e.to_string()),
        }
    } else {
        cfg.api_token = Some(raw_token.clone());
        let client =
            ServerClient::new(cfg.server_url.clone(), raw_token).map_err(|e| e.to_string())?;
        client
            .register(&HlRegisterReq {
                agent_address: agent_address.clone(),
                client_version: Some(format!("degenbox-signer-app {}", env!("CARGO_PKG_VERSION"))),
                host_id: cfg.host_id.clone(),
                // CRITICAL: declare the pairing target on the bearer /
                // Discord path too. The gateway only delivers
                // instructions to rows with `paired_with_account` set —
                // without this the signer registers "Ready" but every
                // trade submit 403s with "no active signer paired".
                paired_with_account: Some(account.clone()),
            })
            .await
            .map_err(|e| e.to_string())?
    };

    // Persist the JWT the redeem flow minted (bearer path already set it).
    if let Some(tok) = &resp.api_token {
        cfg.api_token = Some(tok.clone());
    }
    // Per-wallet config is authoritative; the global `hl-config.json`
    // stays synced to the PRIMARY pairing (CLI signer interop).
    match (&vault, &target_entry) {
        (Some(v), Some(e)) => {
            cfg.save_to_path(&v.hl_config_path(e))?;
            if is_primary_target {
                cfg.save()?;
            }
        }
        _ => cfg.save()?,
    }

    if is_primary_target {
        // Bring the daemon online now if the HL keystore is unlocked.
        if let Some(secret) = state
            .hl_secret_hex
            .lock()
            .map_err(|e| e.to_string())?
            .clone()
        {
            // Re-pairing while a daemon is RUNNING must not half-apply:
            // the old poller still holds the previous token / master /
            // claim scope. Signal it to stop, then spawn fresh — the CAS
            // re-arms immediately and the generation bump makes the old
            // loop exit on its next tick instead of double-polling.
            state
                .hl_runtime
                .daemon_running
                .store(false, Ordering::SeqCst);
            spawn_hl_daemon(&app, secret, cfg);
        }
    } else {
        // Secondary wallet: its standby loop must pick up the fresh
        // pairing — restart the runtime topology.
        crate::clients::restart_runtimes(&app);
    }

    Ok(HlPairResp {
        user_id: resp.user_id,
        agent_address: resp.agent_address,
        discord_handle: resp.discord_handle,
        needs_totp: false,
    })
}

#[tauri::command]
pub fn pick_backend(_backend: core::KeystoreBackend) -> Result<(), String> {
    // Persistence-of-choice is intentionally left for the next
    // iteration. The first run prompts; subsequent runs read from
    // tauri-store on the frontend. Stub is here so the wizard can
    // call it without conditional JS.
    Ok(())
}

// ─────────────────────────── HL surface ───────────────────────────

#[derive(Debug, Serialize)]
pub struct HlStatusReport {
    pub conn: ConnState,
    pub paired: bool,
    pub paper_mode: bool,
    pub user_id: Option<String>,
    pub discord_handle: Option<String>,
    pub agent_address: Option<String>,
    pub account_address: Option<String>,
    pub server_url: String,
    pub network: String,
    pub queue_pending: usize,
    pub last_poll_at: Option<String>,
    pub error: Option<String>,
    pub balance: BalanceSnapshotDto,
    /// A pending TOTP challenge the GUI must answer, if any.
    pub totp_prompt: Option<TotpPrompt>,
}

#[derive(Debug, Serialize)]
pub struct BalanceSnapshotDto {
    pub account_value_usd: Option<String>,
    pub withdrawable_usd: Option<String>,
    pub positions: Vec<PositionDto>,
    pub fetched_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PositionDto {
    pub coin: String,
    pub szi: String,
    pub side: String,
    pub unrealized_pnl: Option<String>,
    pub entry_px: Option<String>,
}

impl From<BalanceSnapshot> for BalanceSnapshotDto {
    fn from(b: BalanceSnapshot) -> Self {
        Self {
            account_value_usd: b.account_value_usd,
            withdrawable_usd: b.withdrawable_usd,
            positions: b
                .positions
                .into_iter()
                .map(|p| PositionDto {
                    coin: p.coin,
                    szi: p.szi,
                    side: p.side,
                    unrealized_pnl: p.unrealized_pnl,
                    entry_px: p.entry_px,
                })
                .collect(),
            fetched_at: b.fetched_at.map(|t| t.to_rfc3339()),
            error: b.error,
        }
    }
}

/// Live HL daemon status for the dashboard. Reads the shared runtime the
/// daemon writes + the persisted config. Never blocks on the network.
#[tauri::command]
pub fn hl_status(state: State<'_, AppState>) -> Result<HlStatusReport, String> {
    let rt = &state.hl_runtime;
    let cfg = HlConfig::load_or_default();
    let paired = cfg.api_token.is_some() && cfg.agent_address.is_some();
    let conn = rt
        .conn
        .lock()
        .ok()
        .and_then(|g| *g)
        .unwrap_or(ConnState::Offline);
    let balance: BalanceSnapshot = rt.balance.lock().map(|g| g.clone()).unwrap_or_default();
    Ok(HlStatusReport {
        conn,
        paired,
        paper_mode: rt.paper_mode.lock().map(|g| *g).unwrap_or(cfg.paper_mode),
        user_id: rt.user_id.lock().ok().and_then(|g| g.clone()),
        discord_handle: rt.discord_handle.lock().ok().and_then(|g| g.clone()),
        agent_address: cfg.agent_address.clone(),
        account_address: cfg.account_address.clone(),
        server_url: cfg.server_url.clone(),
        network: match cfg.network {
            NetworkChoice::Mainnet => "mainnet".into(),
            NetworkChoice::Testnet => "testnet".into(),
        },
        queue_pending: rt.queue_pending.lock().map(|g| *g).unwrap_or(0),
        last_poll_at: rt
            .last_poll_at
            .lock()
            .ok()
            .and_then(|g| *g)
            .map(|t| t.to_rfc3339()),
        error: rt.error.lock().ok().and_then(|g| g.clone()),
        balance: balance.into(),
        totp_prompt: rt.totp_prompt.lock().ok().and_then(|g| g.clone()),
    })
}

/// Submit a TOTP code the daemon is waiting on (per-trade 2FA). The
/// daemon's `wait_for_totp_code` consumes it and re-sends the result with
/// the bypass token.
#[tauri::command]
pub fn submit_hl_totp(code: String, state: State<'_, AppState>) -> Result<(), String> {
    let code = code.trim().to_string();
    if code.is_empty() {
        return Err("empty TOTP code".into());
    }
    let mut g = state
        .hl_runtime
        .totp_answer
        .lock()
        .map_err(|e| e.to_string())?;
    *g = Some(code);
    Ok(())
}

/// Flip the LIVE runtime paper flag for EVERY HL executor — the
/// primary's shared `AppState.hl_runtime` AND each vault client's own
/// runtime (secondaries get their own `HlRuntime` at spawn). The daemon
/// core reads this flag per instruction (`effective_paper_mode`), so
/// the change governs the very next claim of every running daemon
/// without a respawn — and `hl_status` (same flag) reflects it
/// immediately. Covering only the primary here is the audit-N1 bug:
/// secondaries kept trading LIVE under a "paper" badge.
pub fn apply_paper_mode_live(state: &AppState, paper: bool) {
    if let Ok(mut g) = state.hl_runtime.paper_mode.lock() {
        *g = paper;
    }
    if let Ok(clients) = state.clients.lock() {
        for c in clients.iter() {
            // The primary shares `state.hl_runtime` (same Arc) — the
            // re-flip is harmless; every secondary's own flag matters.
            if let Some(rt) = &c.hl_runtime {
                if let Ok(mut g) = rt.paper_mode.lock() {
                    *g = paper;
                }
            }
        }
    }
}

/// Persist the paper flag everywhere a daemon (re)spawn reads it from:
/// the global `hl-config.json` (CLI-shared, the primary's fallback) +
/// EVERY HL wallet's per-wallet vault config when present (authoritative
/// for the next respawn — `hl_config_for` prefers it, and secondaries
/// seed `DaemonOpts::paper_mode` from it, so skipping any would silently
/// revert the toggle for that executor).
fn persist_paper_mode(paper: bool) -> Result<(), String> {
    let mut cfg = HlConfig::load_or_default();
    cfg.paper_mode = paper;
    cfg.save()?;
    if let Ok(Some(vault)) = crate::clients::open_vault() {
        for entry in vault
            .wallets()
            .iter()
            .filter(|w| w.chain == core::WalletChain::Hl)
        {
            let per_wallet = vault.hl_config_path(entry);
            if per_wallet.exists() {
                let mut pc = HlConfig::load_from(&per_wallet);
                pc.paper_mode = paper;
                pc.save_to_path(&per_wallet)
                    .map_err(|e| format!("persist paper mode for {}: {e}", entry.address))?;
            }
        }
    }
    Ok(())
}

/// Toggle paper-mode (dry-run) at runtime — for EVERY HL executor on
/// this device (primary + secondaries). Two halves, ordered fail-closed:
///
/// - paper ON (going safe): flip the live runtime flags FIRST so every
///   daemon's next claim is already dry-run, then persist. A persist
///   failure surfaces as an error but never re-arms live trading.
/// - paper OFF (going live): persist EVERY config first; the live flags
///   flip only after all writes succeed. A persist failure leaves the
///   whole device in paper — never a half-live fleet.
#[tauri::command]
pub fn hl_set_paper_mode(paper: bool, state: State<'_, AppState>) -> Result<(), String> {
    if paper {
        apply_paper_mode_live(&state, true);
        persist_paper_mode(true)
    } else {
        persist_paper_mode(false)?;
        apply_paper_mode_live(&state, false);
        Ok(())
    }
}

/// Server-side pairing truth (`GET /signer/pairing`) — lets the UI
/// refuse to claim "paired" when the gateway disagrees
/// (wallet_mismatch / unpaired / revoked). `Ok(None)` when this device
/// has no pairing token yet, or the gateway predates the endpoint
/// (404) — the UI hides the row instead of erroring.
///
/// `client_id` selects a vault HL wallet's OWN pairing config/token
/// (per-agent view for the fleet UI); default = the device-level
/// (primary) config. The gateway derives the state across the user's
/// agents and reports the BEST row — the UI compares the returned
/// `agent_address` against the client's address before claiming it.
#[tauri::command]
pub async fn hl_pairing_status(
    client_id: Option<String>,
) -> Result<Option<degenbox_signer_core::hl::server::PairingStatus>, String> {
    let cfg = match &client_id {
        Some(id) => {
            let vault = crate::clients::open_vault()?
                .ok_or_else(|| "no vault on this device".to_string())?;
            let entry = vault
                .get(id)
                .cloned()
                .ok_or_else(|| format!("unknown client {id}"))?;
            if entry.chain != core::WalletChain::Hl {
                return Err("client is not a Hyperliquid wallet".into());
            }
            let is_primary = vault
                .primary(core::WalletChain::Hl)
                .is_some_and(|p| p.id == entry.id);
            crate::clients::hl_config_for(&vault, &entry, is_primary)
        }
        None => HlConfig::load_or_default(),
    };
    let Some(token) = cfg.api_token.clone() else {
        return Ok(None);
    };
    let client = ServerClient::new(cfg.server_url.clone(), token).map_err(|e| e.to_string())?;
    match client.pairing().await {
        Ok(p) => Ok(Some(p)),
        Err(ServerError::Status(status, _)) if status.as_u16() == 404 => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Clear the HL pairing (token + master account) from disk. Stops the
/// daemon. The encrypted keystore is left untouched. Operates on the
/// PRIMARY HL wallet (global config + its per-wallet vault config).
#[tauri::command]
pub fn hl_unpair(state: State<'_, AppState>) -> Result<(), String> {
    let mut cfg = HlConfig::load_or_default();
    cfg.api_token = None;
    cfg.account_address = None;
    cfg.save()?;
    if let Ok(Some(vault)) = crate::clients::open_vault() {
        if let Some(primary) = vault.primary(core::WalletChain::Hl) {
            let per_wallet = vault.hl_config_path(primary);
            if per_wallet.exists() {
                cfg.save_to_path(&per_wallet)?;
            }
        }
    }
    state
        .hl_runtime
        .daemon_running
        .store(false, Ordering::SeqCst);
    state.hl_runtime.set_conn(ConnState::Offline);
    Ok(())
}

/// Live status of the `127.0.0.1:5829` signer-protocol daemon (the
/// surface the DegenBox web app probes to detect this client).
/// `error` carries a bind failure verbatim — e.g. the port is held by a
/// running `signer-cli daemon` — so the GUI can tell the user instead
/// of failing silently.
#[tauri::command]
pub fn local_daemon_status(
    state: State<'_, AppState>,
) -> Result<crate::local_daemon::LocalDaemonStatus, String> {
    state
        .local_daemon
        .lock()
        .map(|g| g.clone())
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn open_logs(app: tauri::AppHandle) -> Result<(), String> {
    let path = core::app_log_path().map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if !path.exists() {
        let _ = std::fs::write(&path, b"");
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_path(path.to_string_lossy().to_string(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct OpenSetupReq {
    pub server_url: String,
}

/// Map a gateway/API base URL to the matching WEB FRONTEND origin for
/// every browser-opening flow. `/hl/setup` (and the dashboard in
/// general) exists only on the frontend host — opening it on the API
/// gateway 404s. Known prod hosts are mapped explicitly; anything else
/// falls back to the staging frontend. `DEGENBOX_FRONTEND_URL`
/// overrides everything (local dev / future cutover).
pub fn frontend_base_url(server_url: &str) -> String {
    if let Ok(url) = std::env::var("DEGENBOX_FRONTEND_URL") {
        let trimmed = url.trim().trim_end_matches('/').to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    let s = server_url.trim().trim_end_matches('/').to_ascii_lowercase();
    match s.as_str() {
        // v2 gateway → v2 frontend (staging today, apex on cutover).
        "https://api-v2.degenbox.app" => "https://staging.degenbox.app".into(),
        "https://api.staging.degenbox.app" => "https://staging.degenbox.app".into(),
        // Apex API (v1 today / v2 after cutover) → apex frontend.
        "https://api.degenbox.app" => "https://degenbox.app".into(),
        _ => "https://staging.degenbox.app".into(),
    }
}

#[tauri::command]
pub fn open_setup_url(req: OpenSetupReq, app: tauri::AppHandle) -> Result<(), String> {
    // Browser flow that lands the user at the DegenBox HL setup page
    // with `?return=degenbox://hl/setup-complete?token=...` for a
    // deep-link callback once they've registered. NOTE the page lives on
    // the FRONTEND host, not the API gateway the signer pairs with.
    let url = format!(
        "{}/hl/setup?source=desktop-signer&return={}",
        frontend_base_url(&req.server_url),
        urlencode("degenbox://hl/setup-complete")
    );
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

// ───────────────────── wallet management ─────────────────────

/// Resolve a Solana wallet's vault entry by pubkey (None = primary).
fn sol_entry_for(vault: &core::Vault, pubkey: Option<&str>) -> Result<core::WalletEntry, String> {
    match pubkey {
        Some(pk) => vault
            .wallets()
            .iter()
            .find(|w| w.chain == core::WalletChain::Sol && w.address == pk)
            .cloned()
            .ok_or_else(|| format!("no vault wallet with pubkey {pk}")),
        None => vault
            .primary(core::WalletChain::Sol)
            .cloned()
            .ok_or_else(|| "no Solana wallet set up yet".to_string()),
    }
}

/// Export the encrypted Solana keystore to a user-chosen path (backup
/// step of the wallet wizard + Settings). What leaves the app is the
/// SAME encrypted envelope that sits on disk — never plaintext key
/// material — so this needs no password. `pubkey` selects a specific
/// vault wallet (default: the primary).
#[tauri::command]
pub fn export_sol_keystore(dest: String, pubkey: Option<String>) -> Result<String, String> {
    if let Some(vault) = crate::clients::open_vault()? {
        if !vault.wallets().is_empty() || pubkey.is_some() {
            let entry = sol_entry_for(&vault, pubkey.as_deref())?;
            return crate::clients::client_export_keystore(entry.id, dest);
        }
    }
    // Legacy single-keystore fallback (pre-migration installs).
    let src = core::sol_keystore_path().map_err(|e| e.to_string())?;
    if !src.exists() {
        return Err("no Solana wallet set up yet".into());
    }
    // Parse-validate before writing so we can never export garbage.
    let bytes = std::fs::read(&src).map_err(|e| e.to_string())?;
    let ks: core::Keystore = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let dest_path = std::path::PathBuf::from(dest);
    core::save_to_path(&ks, &dest_path).map_err(|e| e.to_string())?;
    Ok(dest_path.to_string_lossy().into_owned())
}

#[derive(Debug, Deserialize)]
pub struct RevealSolSecretReq {
    pub password: String,
    /// Which vault wallet to reveal (default: the primary).
    #[serde(default)]
    pub pubkey: Option<String>,
}

/// Decrypt and return the Solana secret key (base58, 64-byte expanded
/// form — the shape Phantom/Solflare import). EXPLICIT user action
/// only: requires re-entering the passphrase even while unlocked, and
/// the GUI shows it behind a dedicated reveal click. The string is
/// returned once and never cached on the Rust side.
#[tauri::command]
pub fn reveal_sol_secret(req: RevealSolSecretReq) -> Result<String, String> {
    if let Some(vault) = crate::clients::open_vault()? {
        if !vault.wallets().is_empty() || req.pubkey.is_some() {
            let entry = sol_entry_for(&vault, req.pubkey.as_deref())?;
            let kp = vault
                .unlock_sol(&entry.id, &req.password)
                .map_err(|e| e.to_string())?;
            return Ok(bs58::encode(kp.to_bytes()).into_string());
        }
    }
    let path = core::sol_keystore_path().map_err(|e| e.to_string())?;
    if !path.exists() {
        return Err("no Solana wallet set up yet".into());
    }
    let kp = core::keystore::load_from_path(&path, &req.password).map_err(|e| e.to_string())?;
    Ok(bs58::encode(kp.to_bytes()).into_string())
}

/// Remove the PRIMARY Solana wallet from this machine (legacy
/// single-wallet surface; the Clients page removes by id). The GUI
/// gates this behind a typed confirmation. Vault removals keep the
/// ciphertext as `.removed.bak`; legacy single-file removal deletes.
#[tauri::command]
pub fn remove_sol_keystore(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    if let Some(vault) = crate::clients::open_vault()? {
        if let Some(primary) = vault.primary(core::WalletChain::Sol) {
            let id = primary.id.clone();
            if let Ok(mut g) = state.sol_seed.lock() {
                *g = None;
            }
            state.sol_slot.clear();
            crate::sol::runtime::stop(&state);
            return crate::clients::client_remove(id, app.clone());
        }
    }
    let path = core::sol_keystore_path().map_err(|e| e.to_string())?;
    if !path.exists() {
        return Err("no Solana wallet set up".into());
    }
    if let Ok(mut g) = state.sol_seed.lock() {
        *g = None;
    }
    state.sol_slot.clear();
    crate::sol::runtime::stop(&state);
    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Remove the PRIMARY Hyperliquid agent wallet (legacy single-wallet
/// surface; the Clients page removes by id). Stops the HL daemon and
/// wipes the in-memory secret first. The pairing config (token +
/// master account) survives — re-importing the same agent key resumes;
/// use `hl_unpair` to sever the gateway side.
#[tauri::command]
pub fn remove_hl_keystore(app: tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    if let Some(vault) = crate::clients::open_vault()? {
        if let Some(primary) = vault.primary(core::WalletChain::Hl) {
            let id = primary.id.clone();
            if let Ok(mut g) = state.hl_secret_hex.lock() {
                *g = None;
            }
            state
                .hl_runtime
                .daemon_running
                .store(false, Ordering::SeqCst);
            state.hl_runtime.set_conn(ConnState::Offline);
            return crate::clients::client_remove(id, app.clone());
        }
    }
    let path = core::hl_keystore_path().map_err(|e| e.to_string())?;
    if !path.exists() {
        return Err("no Hyperliquid agent key set up".into());
    }
    if let Ok(mut g) = state.hl_secret_hex.lock() {
        *g = None;
    }
    state
        .hl_runtime
        .daemon_running
        .store(false, Ordering::SeqCst);
    state.hl_runtime.set_conn(ConnState::Offline);
    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    Ok(())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// H11: the toggle command's runtime half — flipping the live flag
    /// is what makes the RUNNING daemon switch on its next instruction
    /// (the core reads `runtime.paper_mode` per claim) and what makes
    /// `hl_status` (same flag) reflect the change immediately.
    #[test]
    fn apply_paper_mode_live_flips_the_shared_runtime_flag() {
        let state = AppState::default();
        assert!(!state.hl_runtime.paper_mode.lock().map(|g| *g).unwrap());

        apply_paper_mode_live(&state, true);
        assert!(
            state.hl_runtime.paper_mode.lock().map(|g| *g).unwrap(),
            "paper ON must land in the live runtime flag"
        );

        apply_paper_mode_live(&state, false);
        assert!(
            !state.hl_runtime.paper_mode.lock().map(|g| *g).unwrap(),
            "paper OFF must land in the live runtime flag"
        );
    }

    /// N1 (audit 2026-06-12): the toggle must cover EVERY HL executor —
    /// secondaries run their OWN `HlRuntime`, and their daemons read
    /// `runtime.paper_mode` per instruction. A primary-only flip leaves
    /// them trading LIVE under a "paper" badge.
    #[test]
    fn apply_paper_mode_live_covers_every_hl_executor() {
        use crate::state::{ClientHandle, ClientRole};
        use std::sync::{atomic::AtomicBool, Arc, Mutex};

        let mk_entry = |id: &str, chain: core::WalletChain| core::WalletEntry {
            id: id.into(),
            chain,
            address: format!("addr-{id}"),
            label: None,
            created_at: chrono::Utc::now(),
            file: format!("{id}.json"),
            paused: false,
        };
        let state = AppState::default();
        let secondary_rt: crate::hl::runtime::SharedHlRuntime =
            Arc::new(crate::hl::runtime::HlRuntime::default());
        {
            let mut clients = state.clients.lock().unwrap();
            // Primary HL wallet — shares the device runtime (same Arc).
            clients.push(ClientHandle {
                entry: mk_entry("hl-primary", core::WalletChain::Hl),
                role: ClientRole::Primary,
                pause_gate: Arc::new(Mutex::new(false)),
                sol_seed: None,
                hl_secret_hex: None,
                hl_runtime: Some(state.hl_runtime.clone()),
                sol_runtime: None,
                hl_executor: Arc::new(AtomicBool::new(true)),
            });
            // Secondary HL wallet — its OWN runtime (the N1 gap).
            clients.push(ClientHandle {
                entry: mk_entry("hl-secondary", core::WalletChain::Hl),
                role: ClientRole::Standby,
                pause_gate: Arc::new(Mutex::new(false)),
                sol_seed: None,
                hl_secret_hex: None,
                hl_runtime: Some(secondary_rt.clone()),
                sol_runtime: None,
                hl_executor: Arc::new(AtomicBool::new(true)),
            });
            // Sol wallet — no HL runtime; must be tolerated.
            clients.push(ClientHandle {
                entry: mk_entry("sol-1", core::WalletChain::Sol),
                role: ClientRole::Standby,
                pause_gate: Arc::new(Mutex::new(false)),
                sol_seed: None,
                hl_secret_hex: None,
                hl_runtime: None,
                sol_runtime: None,
                hl_executor: Arc::new(AtomicBool::new(false)),
            });
        }

        apply_paper_mode_live(&state, true);
        assert!(
            state.hl_runtime.paper_mode.lock().map(|g| *g).unwrap(),
            "primary executor must flip to paper"
        );
        assert!(
            secondary_rt.paper_mode.lock().map(|g| *g).unwrap(),
            "SECONDARY executor must flip to paper too — N1 regression"
        );

        apply_paper_mode_live(&state, false);
        assert!(!state.hl_runtime.paper_mode.lock().map(|g| *g).unwrap());
        assert!(
            !secondary_rt.paper_mode.lock().map(|g| *g).unwrap(),
            "paper OFF must reach the secondary as well"
        );
    }

    /// M22: browser-opening flows must land on the FRONTEND host — the
    /// API gateway has no `/hl/setup` route.
    #[test]
    fn frontend_base_url_maps_gateways_to_web_hosts() {
        // Note: relies on DEGENBOX_FRONTEND_URL being unset in the test
        // environment (we don't mutate process env from tests).
        if std::env::var("DEGENBOX_FRONTEND_URL").is_ok() {
            return;
        }
        assert_eq!(
            frontend_base_url("https://api-v2.degenbox.app"),
            "https://staging.degenbox.app"
        );
        assert_eq!(
            frontend_base_url("https://api-v2.degenbox.app/"),
            "https://staging.degenbox.app",
            "trailing slash tolerated"
        );
        assert_eq!(
            frontend_base_url("https://API-V2.DEGENBOX.APP"),
            "https://staging.degenbox.app",
            "case-insensitive host match"
        );
        assert_eq!(
            frontend_base_url("https://api.degenbox.app"),
            "https://degenbox.app"
        );
        // Unknown gateways degrade to the staging frontend instead of
        // 404ing on the API host.
        assert_eq!(
            frontend_base_url("http://localhost:8090"),
            "https://staging.degenbox.app"
        );
    }
}
