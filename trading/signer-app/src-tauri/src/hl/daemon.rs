//! Thin Tauri adapter around the shared HL daemon core
//! (`degenbox_signer_core::hl::daemon`).
//!
//! The poll/sign/report loop, cursor clamp, executed-marker idempotency,
//! audit log, paper mode, and TOTP-via-runtime handling all live in
//! signer-core (canonical, prod-semantics). This file only bridges the
//! core daemon's [`DaemonEvents`] into Tauri state: every handled
//! instruction lands in the dashboard's recent-signs ring and flips the
//! tray health pill.

use crate::state::{AppState, RecentSign, SignerHealth};
use anyhow::Result;
use chrono::Utc;
use degenbox_signer_core::hl::daemon as core_daemon;
pub use degenbox_signer_core::hl::daemon::ClaimScope as CoreClaimScope;
use degenbox_signer_core::hl::daemon::{payload_asset, payload_kind, DaemonEvents};
use degenbox_signer_core::hl::push::spawn_intent_nudge_subscriber;
use degenbox_signer_core::hl::runtime::SharedHlRuntime;
use degenbox_signer_core::hl::server::{PendingRow, RegisterResp};
use degenbox_signer_core::hl::signing::SignedSubmitResult;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::Manager;

use super::config::HlConfig;

/// Everything the app-side daemon spawn needs. Field set kept identical
/// to the pre-dedupe version so `commands::spawn_hl_daemon` is untouched.
pub struct DaemonOpts {
    pub config: HlConfig,
    pub secret_hex: String,
    pub agent_address: String,
    pub poll_interval: Duration,
    pub paper_mode: bool,
    /// Per-client pause flag (effective gate = global kill-switch OR
    /// the client's own pause; see `clients::recompute_pause_gates`).
    pub pause: Arc<std::sync::Mutex<bool>>,
    pub runtime: SharedHlRuntime,
    /// App handle so we can push recent-signs + health into `AppState`.
    pub app: tauri::AppHandle,
    /// Executed-marker ledger override (per-wallet vault ledgers). The
    /// primary keeps `None` → the shared global `executed.jsonl` the
    /// CLI signer also uses.
    pub executed_path: Option<std::path::PathBuf>,
    /// Which slice of the user's claim queue this daemon owns —
    /// `Unscoped` (legacy single executor), or `Scoped { wallet }` for
    /// the per-wallet multi-client topology. The scope is enforced both
    /// at the poll (`?wallet=`) and per claimed row (the core belt).
    pub claim_scope: core_daemon::ClaimScope,
}

/// Bridges core-daemon events into Tauri-managed `AppState`, and bootstraps
/// the gateway-WS push subscriber that drives sub-second order pickup.
struct TauriEvents {
    app: tauri::AppHandle,
    /// `(gateway_base, signer_token, nudge_tx)` — consumed once on first
    /// register to spawn the push subscriber. `None` when the config has no
    /// `api_token` (the daemon would have failed to register anyway), in
    /// which case pickup stays poll-only.
    push: Option<(String, String, tokio::sync::mpsc::Sender<()>)>,
    /// Guards the one-shot subscriber spawn — `registered` can fire again on
    /// a re-register, but we must not stack a second WS subscriber.
    push_spawned: AtomicBool,
}

impl DaemonEvents for TauriEvents {
    fn instruction_handled(&self, row: &PendingRow, result: &SignedSubmitResult) {
        let state = self.app.state::<AppState>();
        let asset = payload_asset(&row.payload).unwrap_or_else(|| "—".into());
        let kind = payload_kind(&row.payload);
        state.push_recent(RecentSign {
            at: Utc::now(),
            chain: "hl",
            kind: format!("{kind} {asset}").trim().to_string(),
            identifier: result.cloid.clone(),
            status: result.status.clone(),
        });
    }

    fn health_changed(&self, healthy: bool) {
        let h = if healthy {
            SignerHealth::Green
        } else {
            SignerHealth::Amber
        };
        self.app.state::<AppState>().set_health(h);
    }

    fn registered(&self, reg: &RegisterResp) {
        // Spawn the gateway-WS push subscriber exactly once, now that the
        // register round-trip resolved our user id. It feeds the daemon's
        // nudge channel so a queued order is claimed in ~ms instead of
        // waiting out the poll interval; the poll loop stays the safety net.
        if let Some((base, token, tx)) = &self.push {
            if !self.push_spawned.swap(true, Ordering::SeqCst) {
                spawn_intent_nudge_subscriber(
                    base.clone(),
                    token.clone(),
                    reg.user_id.clone(),
                    tx.clone(),
                );
            }
        }
    }
}

pub async fn run(opts: DaemonOpts) -> Result<()> {
    // Push-nudge channel: the gateway-WS subscriber (spawned on register)
    // sends `()` here when an instruction is queued, waking the core poll
    // loop immediately. Depth 8 so a burst of intents coalesces into a
    // single fast poll rather than blocking the subscriber. The poll loop
    // is the source of truth + safety net, so a missed nudge costs at most
    // one poll interval.
    let (nudge_tx, nudge_rx) = tokio::sync::mpsc::channel::<()>(8);

    // Push subscriber inputs — only wired when we hold a signer token (the
    // daemon needs one to register at all, so this is virtually always set).
    let push = opts
        .config
        .api_token
        .clone()
        .map(|token| (opts.config.server_url.clone(), token, nudge_tx));

    let events = Arc::new(TauriEvents {
        app: opts.app.clone(),
        push,
        push_spawned: AtomicBool::new(false),
    });
    core_daemon::run_scoped(
        core_daemon::DaemonOpts {
            config: opts.config,
            secret_hex: opts.secret_hex,
            agent_address: opts.agent_address,
            poll_interval: opts.poll_interval,
            paper_mode: opts.paper_mode,
            pause: opts.pause,
            runtime: opts.runtime,
            events,
            client_version: format!("degenbox-signer-app {}", env!("CARGO_PKG_VERSION")),
        },
        core_daemon::DaemonHooks {
            nudge: Some(nudge_rx),
            executed_path: opts.executed_path,
        },
        opts.claim_scope,
    )
    .await
}
