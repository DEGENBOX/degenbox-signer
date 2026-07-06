//! HL daemon glue — the poll/sign/report money path now lives in
//! `signer-core` (`hl::daemon`, the canonical merge whose semantics
//! follow THIS binary's prod behaviour); this module keeps the pieces
//! that are deliberately host-side:
//!
//! - the supervised **NATS push subscriber** (`hyperliquid.intent.exec.
//!   {user}`, 1→30 s backoff, reconnect-forever) feeding the core
//!   loop's nudge channel — `async-nats` stays out of signer-core;
//! - the **stdin TOTP answerer** for headless runs (GUI hosts answer
//!   the parked `TotpPrompt` from their UI; we answer it from the
//!   terminal) and the TUI's modal does the same in-screen;
//! - the **TUI telemetry bridge** ([`CliEvents`]): recent-orders ring,
//!   queue preview / drained-at, and the branded "ready" banner;
//! - per-bot executed-marker paths for the multi-client hub.

use crate::config::{self, Config};
use crate::tui::app::{OrderRow, RuntimeState};
use anyhow::Result;
use chrono::Utc;
pub use degenbox_signer_core::hl::daemon::ClaimScope;
use degenbox_signer_core::hl::daemon::{
    self as core_daemon, payload_asset, DaemonEvents, DaemonHooks,
};
use degenbox_signer_core::hl::runtime::{HlRuntime, SharedHlRuntime};
use degenbox_signer_core::hl::server::{PendingRow, RegisterResp};
use degenbox_signer_core::hl::signing::SignedSubmitResult;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

/// Shared pause flag: the TUI flips it, the core daemon checks it
/// before claiming/signing/submitting. When `true` the daemon stops
/// pulling and executing instructions but keeps its register heartbeat
/// and balance refresh alive so the client cleanly resumes. Per-client.
/// The type matches the core daemon's pause handle.
pub type SharedPause = Arc<Mutex<bool>>;

/// Shared per-client TUI telemetry the events bridge writes and the
/// TUI reads (recent orders, queue preview, update banner).
pub type SharedRuntime = Arc<Mutex<RuntimeState>>;

pub struct DaemonOpts {
    pub config: Config,
    pub secret_hex: String,
    pub agent_address: String,
    /// Cadence for the server-poll loop (the fallback / source of
    /// truth). NATS push just makes the next poll fire immediately.
    pub poll_interval: Duration,
    /// Optional NATS URL. When set, the daemon also subscribes to
    /// `hyperliquid.intent.exec.{user_id}` for sub-second push.
    pub nats_url: Option<String>,
    /// Per-client pause flag. When set and flipped `true`, the daemon
    /// stops claiming/signing/submitting. `None` for headless runs that
    /// never pause.
    pub pause: Option<SharedPause>,
    /// Per-client TUI telemetry sink. `None` for headless runs.
    pub runtime: Option<SharedRuntime>,
    /// Core runtime telemetry (conn state, balances, queue depth, TOTP
    /// prompt slot). The TUI mirrors it on tick; headless runs hand the
    /// daemon a fresh one.
    pub hl_runtime: SharedHlRuntime,
    /// Dry-run: when `true`, the daemon resolves + logs every instruction
    /// but NEVER POSTs to HL. The result is still reported to the gateway
    /// (status `paper`) so the row finalises. A real safety control, not
    /// cosmetic.
    pub paper_mode: bool,
    /// This bot's own config directory (the folder holding its
    /// `hl-config.json` / `hl-keystore.json`). The executed-instruction
    /// idempotency ledger is stored PER BOT inside this dir
    /// (`<config_dir>/executed.jsonl`) so two hub bots never share one
    /// marker store. `None` falls back to the legacy global path
    /// (single-bot installs only).
    pub config_dir: Option<PathBuf>,
    /// Explicit executed-ledger override — takes precedence over
    /// `config_dir`. Vault wallets use their per-wallet
    /// `vault/hl-<addr>.executed.jsonl` (two daemons must never share
    /// one idempotency ledger).
    pub executed_path: Option<PathBuf>,
    /// Which slice of the user's claim queue this daemon owns.
    /// `Unscoped` = legacy single-executor behaviour. Vault primaries
    /// pass `Scoped { wallet: <HL master>, allow_unstamped: true }` so
    /// an instruction stamped for a DIFFERENT master wallet is refused
    /// instead of signed on the wrong account (core enforces it twice:
    /// `?wallet=` poll filter + the per-row scope belt).
    pub claim_scope: ClaimScope,
    /// Print the branded "ready" banner to stderr on register (headless
    /// daemon parity). The TUI passes `false` and renders state itself.
    pub banner: bool,
    /// Answer per-trade TOTP challenges with a blocking stdin prompt
    /// (headless daemon parity). The TUI passes `false` and answers the
    /// parked prompt with an in-screen modal instead.
    pub stdin_totp: bool,
}

pub async fn run(opts: DaemonOpts) -> Result<()> {
    // NATS push nudge channel — the core loop polls immediately when a
    // message lands. The subscriber itself is spawned lazily once the
    // register round-trip yields our user id (see CliEvents::registered).
    let (nudge_tx, nudge_rx) = tokio::sync::mpsc::channel::<()>(8);

    let events = Arc::new(CliEvents {
        runtime: opts.runtime.clone(),
        banner: opts.banner,
        client_name: opts.config.client_name.clone(),
        account_address: opts.config.account_address.clone(),
        nats: opts.nats_url.clone().map(|url| (url, nudge_tx)),
        nats_spawned: AtomicBool::new(false),
    });

    // PER-BOT executed-marker ledger (idempotency): each hub bot keys
    // its own `<bot_dir>/executed.jsonl`; single-bot installs keep the
    // legacy global file (which core also defaults to — passing it
    // explicitly just pins the resolution here).
    let executed_path = match (opts.executed_path, opts.config_dir.as_deref()) {
        (Some(p), _) => Some(p),
        (None, Some(dir)) => Some(config::executed_path_in(dir)),
        (None, None) => config::default_executed_path().ok(),
    };

    // Headless TOTP path: watch the parked prompt and answer from stdin.
    if opts.stdin_totp {
        spawn_stdin_totp_answerer(opts.hl_runtime.clone());
    }

    // The core loop exits when this flag clears (GUI lock/relock); the
    // CLI runs daemon-per-process, so it stays set for our lifetime.
    opts.hl_runtime
        .daemon_running
        .store(true, Ordering::Relaxed);

    let core_opts = core_daemon::DaemonOpts {
        config: opts.config,
        secret_hex: opts.secret_hex,
        agent_address: opts.agent_address,
        poll_interval: opts.poll_interval,
        paper_mode: opts.paper_mode,
        pause: opts.pause.unwrap_or_else(|| Arc::new(Mutex::new(false))),
        runtime: opts.hl_runtime,
        events,
        client_version: format!("hl-signer-desktop {}", env!("CARGO_PKG_VERSION")),
    };
    core_daemon::run_scoped(
        core_opts,
        DaemonHooks {
            nudge: Some(nudge_rx),
            executed_path,
        },
        opts.claim_scope,
    )
    .await
}

/// Host-side event bridge: core daemon → TUI telemetry + banner + NATS
/// subscriber bootstrap.
struct CliEvents {
    runtime: Option<SharedRuntime>,
    banner: bool,
    client_name: Option<String>,
    account_address: Option<String>,
    /// `(nats_url, nudge_tx)` — consumed once on first register.
    nats: Option<(String, tokio::sync::mpsc::Sender<()>)>,
    nats_spawned: AtomicBool,
}

impl DaemonEvents for CliEvents {
    fn instruction_handled(&self, _row: &PendingRow, _result: &SignedSubmitResult) {
        // The daemon calls only the timed variant below.
    }

    fn instruction_handled_timed(&self, row: &PendingRow, result: &SignedSubmitResult, ms: u64) {
        if let Some(rt) = &self.runtime {
            if let Ok(mut s) = rt.lock() {
                s.push_order(OrderRow {
                    at: Utc::now(),
                    coin: payload_asset(&row.payload).unwrap_or_else(|| "-".into()),
                    side: payload_side(&row.payload).unwrap_or_else(|| "-".into()),
                    size_usd: result.filled_size_usd.clone().unwrap_or_else(|| "-".into()),
                    status: result.status.clone(),
                    fill_ms: Some(ms),
                    pnl: result.closed_pnl.clone(),
                });
            }
        }
    }

    fn health_changed(&self, _healthy: bool) {}

    fn registered(&self, reg: &RegisterResp) {
        // Spawn the supervised NATS push subscriber exactly once, now
        // that the register round-trip resolved our user id.
        if let Some((url, tx)) = &self.nats {
            if !self.nats_spawned.swap(true, Ordering::SeqCst) {
                let url = url.clone();
                let user_id = reg.user_id.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    supervise_nats_subscriber(url, user_id, tx).await;
                });
            }
        }
        if !self.banner {
            return;
        }
        // Branded one-liner so operators see a clear "we're live" marker
        // separate from the tracing stream.
        let user_display = reg.discord_handle.as_deref().unwrap_or(&reg.user_id);
        let account_display = self.account_address.as_deref().unwrap_or("not linked");
        if let Some(name) = &self.client_name {
            eprintln!(
                "  {} {}  {} {} {} {} {} {} {} {}",
                crate::branding::brand_tag(),
                crate::branding::status_pill("ready"),
                crate::branding::muted("bot"),
                crate::branding::accent_bold(name),
                crate::branding::muted("·  user"),
                crate::branding::accent_bold(user_display),
                crate::branding::muted("·  wallet"),
                crate::branding::accent_bold(account_display),
                crate::branding::muted("·  agent"),
                crate::branding::accent_bold(&reg.agent_address)
            );
        } else {
            eprintln!(
                "  {} {}  {} {} {} {} {} {}",
                crate::branding::brand_tag(),
                crate::branding::status_pill("ready"),
                crate::branding::muted("user"),
                crate::branding::accent_bold(user_display),
                crate::branding::muted("·  wallet"),
                crate::branding::accent_bold(account_display),
                crate::branding::muted("·  agent"),
                crate::branding::accent_bold(&reg.agent_address)
            );
        }
    }

    fn batch_received(&self, rows: &[PendingRow]) {
        if let Some(rt) = &self.runtime {
            if let Ok(mut s) = rt.lock() {
                s.queue.next_preview = preview_of(rows.first());
            }
        }
    }

    fn batch_settled(&self) {
        if let Some(rt) = &self.runtime {
            if let Ok(mut s) = rt.lock() {
                s.queue.last_drained_at = Some(Utc::now());
                s.queue.next_preview = None;
            }
        }
    }
}

/// Pull a human side label off the instruction payload for telemetry.
fn payload_side(payload: &serde_json::Value) -> Option<String> {
    if let Some(s) = payload.get("side").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    payload
        .get("is_buy")
        .and_then(|v| v.as_bool())
        .map(|b| if b { "buy" } else { "sell" }.to_string())
}

/// Human one-liner for a pending row, e.g. "BTC buy" or "closePosition BTC".
fn preview_of(row: Option<&PendingRow>) -> Option<String> {
    let row = row?;
    let kind = row
        .payload
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("order");
    let asset = payload_asset(&row.payload).unwrap_or_default();
    let side = payload_side(&row.payload).unwrap_or_default();
    Some(
        format!("{kind} {asset} {side}")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Background task for headless runs: when the core daemon parks a
/// per-trade TOTP challenge in the shared runtime, prompt for the code
/// on stdin (blocking, off the executor) and hand the answer back. The
/// core's bounded wait (90 s) acks a terminal `failed` if the operator
/// never answers, so the gateway row can't dangle.
fn spawn_stdin_totp_answerer(rt: SharedHlRuntime) {
    tokio::spawn(async move {
        let mut answered: Option<String> = None;
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let prompt = rt.totp_prompt.lock().ok().and_then(|g| g.as_ref().cloned());
            let Some(p) = prompt else { continue };
            if answered.as_deref() == Some(p.challenge_id.as_str()) {
                continue; // already prompted for this challenge
            }
            answered = Some(p.challenge_id.clone());
            let expires = p.expires_at.clone();
            let code =
                tokio::task::spawn_blocking(move || crate::server::prompt_totp_stdin(&expires))
                    .await
                    .unwrap_or(None);
            if let Some(code) = code {
                if let Ok(mut g) = rt.totp_answer.lock() {
                    *g = Some(code);
                }
            } else {
                warn!("TOTP prompt cancelled — instruction will be acked failed");
            }
        }
    });
}

/// Fresh core runtime for headless runs (no TUI mirroring needed).
pub fn fresh_hl_runtime() -> SharedHlRuntime {
    Arc::new(HlRuntime::default())
}

/// Reconnect-forever supervisor around [`run_nats_subscriber`]. Any
/// connect/subscribe error OR a clean stream-end backs off and retries:
/// 1s, doubling, capped at 30s (the same envelope v1's Go client used for
/// its WS relay). A subscription that actually ran resets the backoff so a
/// brief outage recovers fast. This task lives for the whole daemon — it
/// never gives up — because losing push permanently would silently
/// degrade every trade to poll-interval latency.
async fn supervise_nats_subscriber(
    url: String,
    user_id: String,
    nudge: tokio::sync::mpsc::Sender<()>,
) {
    const MAX_BACKOFF: Duration = Duration::from_secs(30);
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_nats_subscriber(&url, &user_id, &nudge).await {
            Ok(()) => {
                warn!("NATS push stream ended — reconnecting");
                backoff = Duration::from_secs(1);
            }
            Err(e) => {
                warn!(
                    ?e,
                    backoff_secs = backoff.as_secs(),
                    "NATS push connect/subscribe failed — backing off"
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = next_backoff(backoff, MAX_BACKOFF);
    }
}

/// Double the backoff, capped. Pure so the 1s→2s→…→30s schedule is
/// unit-testable.
fn next_backoff(current: Duration, max: Duration) -> Duration {
    (current * 2).min(max)
}

async fn run_nats_subscriber(
    url: &str,
    user_id: &str,
    nudge: &tokio::sync::mpsc::Sender<()>,
) -> Result<()> {
    // `async_nats::connect` returns a client that auto-reconnects
    // internally and re-establishes this subscription across transient
    // drops; the stream only ends if the client is fully closed, which the
    // supervisor above then retries.
    let client = async_nats::connect(url).await?;
    let subject = format!("hyperliquid.intent.exec.{user_id}");
    let mut sub = client.subscribe(subject.clone()).await?;
    info!(%subject, "NATS subscribed for push nudges");
    while let Some(_msg) = sub.next().await {
        let _ = nudge.try_send(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensure the subscribe subject pattern matches the server's
    /// publish subject exactly. A typo here means push nudges silently
    /// stop working.
    #[test]
    fn nats_subject_pattern_matches_server_format() {
        let user_id = "1234abcd";
        let subject = format!("hyperliquid.intent.exec.{user_id}");
        assert_eq!(subject, "hyperliquid.intent.exec.1234abcd");
    }

    #[test]
    fn nats_backoff_doubles_then_caps_at_30s() {
        let max = Duration::from_secs(30);
        let mut b = Duration::from_secs(1);
        let mut seq = vec![b];
        for _ in 0..6 {
            b = next_backoff(b, max);
            seq.push(b);
        }
        // 1 → 2 → 4 → 8 → 16 → 30 (cap) → 30 (stays capped).
        assert_eq!(
            seq.iter().map(|d| d.as_secs()).collect::<Vec<_>>(),
            vec![1, 2, 4, 8, 16, 30, 30]
        );
    }

    #[test]
    fn preview_reads_kind_asset_and_side() {
        let row = PendingRow {
            id: "i".into(),
            cloid: "c".into(),
            payload: serde_json::json!({"kind": "order", "asset": "BTC", "is_buy": true}),
            created_at: Utc::now(),
            target_wallet: None,
        };
        assert_eq!(preview_of(Some(&row)).as_deref(), Some("order BTC buy"));
        assert!(preview_of(None).is_none());
    }
}
