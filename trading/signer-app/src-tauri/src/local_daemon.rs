//! Lifecycle wrapper around signer-core's `127.0.0.1:5829`
//! signer-protocol daemon.
//!
//! The actual HTTP server (frozen `signer-protocol` RPC: `/health`,
//! `/connect`, `/status`, `/quote`, `/swap`, `/setAuth`, `/setGateway`,
//! `/bot/enable`, `/bot/disable`) lives in
//! `degenbox_signer_core::local_daemon` — the same implementation
//! lineage `signer-cli daemon` serves, so the DegenBox web app detects
//! the Tauri client exactly like the existing desktop signer.
//!
//! This module only owns the app-side lifecycle:
//!
//! - spawn on launch (before unlock — `/health` answers immediately so
//!   the web app's probe flips to "connected"; key-requiring endpoints
//!   return `503 locked` until the user unlocks),
//! - surface running/port/error in [`LocalDaemonStatus`] so the GUI can
//!   show a port-conflict instead of failing silently,
//! - the keypair itself flows through `AppState.sol_slot`
//!   (install on unlock, clear on lock).

use crate::state::AppState;
use degenbox_signer_core::{local_daemon_default_port, serve_local_daemon, LocalDaemonState};
use serde::Serialize;
use tauri::Manager;

/// Live `:5829` daemon status for the GUI. `error` carries the bind
/// failure verbatim (e.g. port already in use by `signer-cli daemon`).
#[derive(Debug, Clone, Serialize)]
pub struct LocalDaemonStatus {
    pub running: bool,
    pub port: u16,
    pub error: Option<String>,
}

impl Default for LocalDaemonStatus {
    fn default() -> Self {
        Self {
            running: false,
            port: local_daemon_default_port(),
            error: None,
        }
    }
}

/// Default gateway base the daemon relays intents to until the web app
/// overrides it via `POST /setGateway`.
fn default_gateway() -> String {
    std::env::var("DEGENBOX_GATEWAY_URL").unwrap_or_else(|_| "https://api-v2.degenbox.app".into())
}

/// Solana RPC for route discovery (PumpFun PDA lookups), the blockhash
/// cache, and the pre-sign simulator. Public mainnet-beta is
/// rate-limited; operators can point at a paid endpoint via env.
fn default_rpc_url() -> String {
    std::env::var("SOLANA_RPC_URL").unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into())
}

/// Spawn the `:5829` signer-protocol server for the app's lifetime.
/// Called once from Tauri `setup`. A bind failure (port conflict) is
/// recorded in `AppState.local_daemon` + logged — never silent.
pub fn spawn(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let slot = state.sol_slot.clone();
    let status = state.local_daemon.clone();
    let web_auth = state.web_auth.clone();
    let port = local_daemon_default_port();
    tauri::async_runtime::spawn(async move {
        let daemon_state = LocalDaemonState::new(slot, default_gateway(), default_rpc_url());
        // Seed the daemon's auth with a persisted Discord desktop login
        // (if any) so web-app swaps authenticate from boot without a
        // manual /setAuth push. A later /setAuth still overrides.
        if let Some(auth) = crate::auth::DesktopAuth::load_valid() {
            daemon_state.config.write().await.auth_token = Some(auth.token);
        }
        // Expose the daemon's runtime config (gateway base + the JWT
        // the web app pushes via /setAuth) so the Solana gateway reads
        // can fall back to it when no HL pairing token exists.
        if let Ok(mut g) = web_auth.lock() {
            *g = Some(daemon_state.config.clone());
        }
        if let Ok(mut g) = status.lock() {
            g.running = true;
            g.port = port;
            g.error = None;
        }
        // `serve` only returns on bind failure or fatal server error.
        let res = serve_local_daemon(daemon_state, port).await;
        if let Ok(mut g) = status.lock() {
            g.running = false;
            g.error = res
                .as_ref()
                .err()
                .map(|e| format!("{e:#}"))
                .or(Some("daemon stopped unexpectedly".into()));
        }
        if let Err(e) = res {
            tracing::error!(error = %format!("{e:#}"), port, "signer-protocol daemon failed");
        }
    });
}
