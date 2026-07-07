//! HL signing daemon loop — the canonical poll/sign/report core.
//!
//! Canonical merge of `hl-signer-desktop/src/daemon.rs` (the live prod
//! executor — its semantics win on any divergence) and the signer-app
//! port. Transport-agnostic: host UIs (Tauri app, TUI) plug in via
//! [`DaemonEvents`] + the shared [`crate::hl::runtime::HlRuntime`]
//! telemetry instead of compiling UI types into the money path.
//!
//! Load-bearing behaviours preserved from prod:
//!
//! - register (heartbeat) so the gateway flips our `signer/status` ready;
//! - poll `/instructions/pending` (claim-on-read), sign each with the
//!   EIP-712 phantom-agent signer via `platform-hl-exchange`, POST to HL,
//!   report via `/order/result`;
//! - the same-instant poll-cursor clamp so a same-`created_at` sibling of
//!   a failed row is never stranded ([`commit_cursor`]);
//! - the per-client PAUSE gate (skip the whole poll while paused so a
//!   claim-on-read can't mark a row delivered then drop it);
//! - the restart-durable executed-marker so a `post_result` failure never
//!   re-fires an order ([`crate::hl::exec_state`]);
//! - the local append-only audit log (`audit.jsonl`) of every sign;
//! - a paper-mode dry-run that resolves+reports without POSTing to HL.
//!
//! Per-trade TOTP is answered through the host UI: a 428 parks a
//! [`TotpPrompt`] in the shared runtime and the daemon waits up to a
//! bounded window for the host to fill the answer. (The CLI's stdin
//! prompt stays in `hl-signer-desktop`.)

use crate::hl::audit::{AuditEntry, AuditLog};
use crate::hl::config::{audit_path, executed_path, HlConfig, NetworkChoice};
use crate::hl::exec_state::ExecutedStore;
use crate::hl::info::HttpInfoClient;
use crate::hl::runtime::{BalanceSnapshot, ConnState, PositionRow, SharedHlRuntime, TotpPrompt};
use crate::hl::server::{
    PendingRow, RegisterReq, RegisterResp, ResultReq, ServerClient, ServerError,
};
use crate::hl::signing::{execute, ExecContext, SignError, SignedSubmitResult};
use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use platform_hl_exchange::{AgentSigner, ExchangeClient, Network};
use reqwest::StatusCode;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error, info, warn};

/// Host-UI hooks the daemon fires as it works. Implementations must be
/// cheap + non-blocking — they run inline on the money path.
///
/// The methods with default bodies are ADDITIVE host hooks (banner /
/// queue-preview / per-row timing for the CLI TUI); existing hosts that
/// only implement the two required methods keep compiling unchanged.
pub trait DaemonEvents: Send + Sync {
    /// A pending row was fully handled (executed/replayed + reported).
    fn instruction_handled(&self, row: &PendingRow, result: &SignedSubmitResult);
    /// Coarse health signal for tray/status surfaces. `true` = healthy,
    /// `false` = degraded (poll/report failures, retrying).
    fn health_changed(&self, healthy: bool);
    /// Registration with the gateway succeeded. Hosts can print a ready
    /// banner / start push subscribers keyed on `resp.user_id`.
    fn registered(&self, _resp: &RegisterResp) {}
    /// A non-empty pending batch was claimed (fired before handling
    /// starts). Hosts can preview the first instruction.
    fn batch_received(&self, _rows: &[PendingRow]) {}
    /// The current batch finished (drained or stopped on a failed row)
    /// and the poll cursor was committed.
    fn batch_settled(&self) {}
    /// Like [`Self::instruction_handled`] but carrying the wall-clock
    /// milliseconds the row took to execute+report. Default delegates so
    /// hosts opt in. The daemon calls ONLY this variant.
    fn instruction_handled_timed(&self, row: &PendingRow, result: &SignedSubmitResult, _ms: u64) {
        self.instruction_handled(row, result);
    }
}

/// No-op events sink for headless hosts.
pub struct NoEvents;
impl DaemonEvents for NoEvents {
    fn instruction_handled(&self, _row: &PendingRow, _result: &SignedSubmitResult) {}
    fn health_changed(&self, _healthy: bool) {}
}

/// Everything the daemon needs to run. Built by the host from the
/// unlocked secret + loaded config + shared handles.
pub struct DaemonOpts {
    pub config: HlConfig,
    pub secret_hex: String,
    pub agent_address: String,
    pub poll_interval: Duration,
    pub paper_mode: bool,
    /// Per-client pause flag (shared with the host UI's pause toggle).
    pub pause: Arc<std::sync::Mutex<bool>>,
    pub runtime: SharedHlRuntime,
    /// Host-UI event hooks (recent-signs ring, tray health, …).
    pub events: Arc<dyn DaemonEvents>,
    /// `client_version` reported on register, e.g.
    /// `"degenbox-signer-app 0.1.0"`.
    pub client_version: String,
}

/// Optional host plumbing for [`run_with`]. Additive so existing
/// [`run`] callers (the Tauri app) are untouched.
#[derive(Default)]
pub struct DaemonHooks {
    /// Push-nudge channel: a message makes the next poll fire NOW
    /// instead of waiting out the ticker. The CLI feeds this from its
    /// supervised NATS subscriber (`hyperliquid.intent.exec.{user}`) so
    /// push latency stays sub-second; transport stays host-side to keep
    /// `async-nats` out of signer-core.
    pub nudge: Option<tokio::sync::mpsc::Receiver<()>>,
    /// Executed-marker ledger override. The multi-client CLI hub passes
    /// the per-bot `<bot_dir>/executed.jsonl` so two hub bots never
    /// share one marker store; `None` keeps the shared global path
    /// (single-instance installs + the desktop app).
    pub executed_path: Option<std::path::PathBuf>,
}

/// Which slice of the user's claim queue this daemon owns.
///
/// Multi-client gateways stamp every instruction with the HL MASTER
/// wallet it must execute on (`target_wallet`) and accept a `?wallet=`
/// claim filter. N per-wallet daemons then each claim exactly their own
/// work. The scope is enforced TWICE: server-side via the query param,
/// and client-side via [`ClaimScope::row_in_scope`] — because an OLD
/// gateway silently ignores the unknown query param, and a scoped
/// daemon must never execute (or worse, mis-account) another wallet's
/// instruction that such a gateway hands it.
#[derive(Debug, Clone, Default)]
pub enum ClaimScope {
    /// Legacy single-executor behaviour: claim + execute everything the
    /// gateway delivers for this user (the CLI signer's mode).
    #[default]
    Unscoped,
    /// Claim with `?wallet=` and execute ONLY rows stamped for `wallet`.
    /// `allow_unstamped` additionally accepts rows WITHOUT a
    /// `target_wallet` stamp — the designated PRIMARY wallet sets this
    /// so legacy rows (old gateway / pre-stamp backlog) keep executing
    /// exactly once, on the wallet that always owned them; secondary
    /// executors keep it `false` (an unstamped row reaching a scoped
    /// secondary means the gateway ignored the filter).
    Scoped {
        /// HL MASTER wallet (0x…, case-insensitive).
        wallet: String,
        allow_unstamped: bool,
    },
}

impl ClaimScope {
    /// The `?wallet=` value to poll with (`None` = unscoped poll).
    pub fn poll_wallet(&self) -> Option<&str> {
        match self {
            ClaimScope::Unscoped => None,
            ClaimScope::Scoped { wallet, .. } => Some(wallet.as_str()),
        }
    }

    /// May this daemon execute a row stamped `target_wallet`? Pure —
    /// the double-execution / wrong-account safety belt.
    pub fn row_in_scope(&self, target_wallet: Option<&str>) -> bool {
        match self {
            ClaimScope::Unscoped => true,
            ClaimScope::Scoped {
                wallet,
                allow_unstamped,
            } => match target_wallet {
                Some(t) => t.eq_ignore_ascii_case(wallet),
                None => *allow_unstamped,
            },
        }
    }
}

pub async fn run(opts: DaemonOpts) -> Result<()> {
    run_with(opts, DaemonHooks::default()).await
}

pub async fn run_with(opts: DaemonOpts, hooks: DaemonHooks) -> Result<()> {
    run_scoped(opts, hooks, ClaimScope::Unscoped).await
}

/// [`run_with`] + a wallet [`ClaimScope`]. Additive entry point so the
/// CLI's literal `DaemonHooks { … }` construction keeps compiling; the
/// multi-client app passes a per-wallet scope here.
pub async fn run_scoped(opts: DaemonOpts, hooks: DaemonHooks, scope: ClaimScope) -> Result<()> {
    // Capture our run generation — see `HlRuntime::run_generation`. A
    // later spawn on the same runtime bumps it; this loop then exits
    // even when `daemon_running` was re-armed by the newer spawn.
    let my_generation = opts.runtime.run_generation.load(Ordering::SeqCst);
    let network = match opts.config.network {
        NetworkChoice::Mainnet => Network::Mainnet,
        NetworkChoice::Testnet => Network::Testnet,
    };
    let signer = AgentSigner::from_hex(&opts.secret_hex, network)
        .map_err(|e| anyhow!("agent signer: {e}"))?;
    if !signer
        .address_hex()
        .eq_ignore_ascii_case(&opts.agent_address)
    {
        return Err(anyhow!(
            "keystore unlocked an address different from config: {} vs {}",
            signer.address_hex(),
            opts.agent_address
        ));
    }
    let hl_client = ExchangeClient::new(network).map_err(|e| anyhow!("hl client: {e}"))?;
    let info_client = HttpInfoClient::new(network).map_err(|e| anyhow!("hl info client: {e}"))?;

    let api_token = opts
        .config
        .api_token
        .clone()
        .ok_or_else(|| anyhow!("api_token missing — register the signer first"))?;
    let server = ServerClient::new(opts.config.server_url.clone(), api_token)?;

    opts.runtime.set_conn(ConnState::Connecting);

    let host_id = opts
        .config
        .host_id
        .clone()
        .or_else(|| std::env::var("HOSTNAME").ok().or_else(hostname_fallback));
    let reg = server
        .register(&RegisterReq {
            agent_address: opts.agent_address.clone(),
            client_version: Some(opts.client_version.clone()),
            host_id,
            // Re-assert the pairing on every daemon boot — the gateway
            // only delivers instructions to rows with this set, and it
            // coalesce-preserves an existing value when None.
            paired_with_account: opts.config.account_address.clone(),
        })
        .await?;
    info!(user_id = %reg.user_id, agent = %reg.agent_address, "registered with gateway");
    opts.events.registered(&reg);

    if let Ok(mut g) = opts.runtime.user_id.lock() {
        *g = Some(reg.user_id.clone());
    }
    if let Ok(mut g) = opts.runtime.discord_handle.lock() {
        *g = reg.discord_handle.clone();
    }
    if let Ok(mut g) = opts.runtime.agent_address.lock() {
        *g = Some(reg.agent_address.clone());
    }
    if let Ok(mut g) = opts.runtime.account_address.lock() {
        *g = opts.config.account_address.clone();
    }
    if let Ok(mut g) = opts.runtime.paper_mode.lock() {
        *g = opts.paper_mode;
    }
    opts.runtime.set_conn(ConnState::Ready);
    opts.events.health_changed(true);

    // Background balance refresher off the MASTER account (the agent reads
    // $0). Runs every 5s into the shared runtime. Skipped + surfaced when
    // no master is paired.
    //
    // A depth-1 nudge channel lets the poll loop request an IMMEDIATE
    // balance refresh the moment it signs an instruction — otherwise the
    // app's Positions/Account views (fed by this snapshot) keep showing the
    // pre-fill balance/size until the next 5 s tick after a trade the user
    // just placed. Depth 1 coalesces a burst into one extra fetch; a full
    // channel drops the nudge (the ticker still catches up ≤ 5 s later).
    let (bal_nudge_tx, bal_nudge_rx) = tokio::sync::mpsc::channel::<()>(1);
    if let Some(master) = opts.config.account_address.clone() {
        let rt = opts.runtime.clone();
        let net = network;
        tokio::spawn(async move {
            balance_refresh_loop(net, master, rt, my_generation, bal_nudge_rx).await;
        });
    } else if let Ok(mut g) = opts.runtime.balance.lock() {
        // Not fatal — only closePosition / placeTpsl require the master,
        // and those payloads return a clean error if they land.
        warn!(
            "no account_address configured — Close / TP / SL instructions will fail until paired"
        );
        g.error = Some("account not paired — link your HL master wallet (0x…)".into());
    }

    let ctx = ExecContext {
        signer: Arc::new(signer),
        hl: hl_client,
        info: Arc::new(info_client),
        account_address: opts.config.account_address.clone(),
        // M24: refuse instructions pinned to a different network.
        network_tag: if network.is_mainnet() {
            "mainnet"
        } else {
            "testnet"
        },
    };

    // Local append-only audit log — one JSONL line per signed+submitted
    // instruction. Best-effort: if it can't be opened we log once and
    // run without it rather than refusing to sign.
    let audit = audit_path().ok().and_then(|p| match AuditLog::open(&p) {
        Ok(a) => Some(a),
        Err(e) => {
            warn!(?e, "could not open local audit log — continuing without it");
            None
        }
    });

    // Local executed-instruction marker store — the idempotency ledger
    // that stops a `post_result` failure from re-submitting an already-
    // executed order/cancel/closePosition to HL on the next poll.
    // Hosts may override the path (per-bot ledgers in the CLI hub).
    let executed = hooks
        .executed_path
        .map(Ok)
        .unwrap_or_else(|| executed_path().map_err(|e| e.to_string()))
        .ok()
        .and_then(|p| match ExecutedStore::open(&p) {
            Ok(s) => Some(s),
            Err(e) => {
                warn!(
                    ?e,
                    path = %p.display(),
                    "could not open executed-marker store — post_result retries may re-submit"
                );
                None
            }
        });

    let mut last_seen: Option<DateTime<Utc>> = None;
    // Bounded consecutive-failure counter. A row that executed but whose
    // post_result keeps failing would otherwise loop forever silently —
    // surface a permanently-stuck report loudly instead.
    let mut consecutive_failures: u32 = 0;
    const FAILURE_ALERT_THRESHOLD: u32 = 5;
    let mut ticker = interval(opts.poll_interval);

    // Nudge channel: fall back to a never-firing channel whose sender we
    // keep alive for the whole loop, so the select arm below is uniform.
    let (_nudge_keepalive, fallback_rx) = tokio::sync::mpsc::channel::<()>(1);
    let mut nudge_rx = hooks.nudge.unwrap_or(fallback_rx);

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            Some(_) = nudge_rx.recv() => {
                debug!("nudged by host push channel — polling now");
            }
        }

        // STOP gate: if the daemon-running flag was cleared (lock/relock)
        // OR a newer spawn superseded this generation, exit so exactly
        // one poller owns the claim queue.
        if !opts.runtime.daemon_running.load(Ordering::Relaxed)
            || opts.runtime.run_generation.load(Ordering::SeqCst) != my_generation
        {
            info!("daemon stop requested — exiting poll loop");
            return Ok(());
        }

        // PAUSE gate: while paused do NOT claim/sign/submit. Claim-on-read
        // would otherwise mark a row delivered then we'd never execute it,
        // so skip the whole poll. Balance refresh keeps running.
        if *opts.pause.lock().unwrap_or_else(|e| e.into_inner()) {
            opts.runtime.set_conn(ConnState::Paused);
            continue;
        }

        // H9: re-deliver any executed-but-unreported results BEFORE
        // claiming new work. Each result the gateway never received is a
        // row it will otherwise reclaim-expire as "signer offline" — a
        // lie for a genuinely FILLED order. One attempt per cloid per
        // cycle; success clears the durable undelivered marker.
        if let Some(store) = executed.as_ref() {
            flush_undelivered(&server, store, &opts.runtime).await;
        }

        match server
            .pending_scoped(last_seen, 20, scope.poll_wallet())
            .await
        {
            Ok(rows) => {
                opts.runtime.set_error(None);
                if let Ok(mut g) = opts.runtime.last_poll_at.lock() {
                    *g = Some(Utc::now());
                }
                if !*opts.pause.lock().unwrap_or_else(|e| e.into_inner()) {
                    opts.runtime.set_conn(ConnState::Ready);
                }
                if rows.is_empty() {
                    if let Ok(mut g) = opts.runtime.queue_pending.lock() {
                        *g = 0;
                    }
                    continue;
                }
                info!(count = rows.len(), "received pending instructions");
                if let Ok(mut g) = opts.runtime.queue_pending.lock() {
                    *g = rows.len();
                }
                opts.events.batch_received(&rows);

                let total = rows.len();
                // The gateway's `pending(since)` filter is a STRICT
                // `created_at > since` — see `commit_cursor` for the
                // same-instant-sibling hazard this guards.
                let mut handled_cursor: Option<DateTime<Utc>> = None;
                let mut failed_at: Option<DateTime<Utc>> = None;
                for (idx, row) in rows.iter().enumerate() {
                    // SCOPE BELT: never execute a row stamped for another
                    // wallet (or unstamped, on a strict secondary). Can only
                    // trigger against a gateway that ignored our `?wallet=`
                    // filter — report a terminal failure so the row doesn't
                    // dangle, and advance past it WITHOUT signing.
                    if !scope.row_in_scope(row.target_wallet.as_deref()) {
                        error!(
                            cloid = %row.cloid, target_wallet = ?row.target_wallet,
                            scope = ?scope,
                            "claimed instruction is OUT OF SCOPE for this wallet — refusing to sign (gateway ignored the wallet filter?)"
                        );
                        let fail = ResultReq {
                            cloid: row.cloid.clone(),
                            oid: None,
                            status: "failed".into(),
                            filled_size_usd: None,
                            closed_pnl: None,
                            err_msg: Some(
                                "wallet_scope_mismatch — claimed by a wallet-scoped signer the instruction is not stamped for"
                                    .into(),
                            ),
                            signed_at: None,
                            posted_to_hl_at: None,
                        };
                        match post_result_resilient(&server, &fail, &opts.runtime).await {
                            Ok(()) => {
                                handled_cursor = Some(
                                    handled_cursor.unwrap_or(row.created_at).max(row.created_at),
                                );
                            }
                            Err(e) => {
                                warn!(?e, cloid = %row.cloid, "could not report out-of-scope row — holding cursor");
                                failed_at = Some(row.created_at);
                                break;
                            }
                        }
                        opts.runtime.set_error(Some(format!(
                            "refused out-of-scope instruction {} (stamped {:?})",
                            row.cloid, row.target_wallet
                        )));
                        if let Ok(mut g) = opts.runtime.queue_pending.lock() {
                            *g = total.saturating_sub(idx + 1);
                        }
                        continue;
                    }
                    let started = std::time::Instant::now();
                    let outcome = handle_one(
                        &ctx,
                        &server,
                        row,
                        audit.as_ref(),
                        executed.as_ref(),
                        // LIVE flag, not the boot value — the host's
                        // paper toggle must apply to the very next
                        // instruction without a daemon respawn.
                        effective_paper_mode(&opts.runtime, opts.paper_mode),
                        &opts.runtime,
                    )
                    .await;
                    let fill_ms = started.elapsed().as_millis() as u64;
                    match outcome {
                        Ok(out) => {
                            consecutive_failures = 0;
                            handled_cursor =
                                Some(handled_cursor.unwrap_or(row.created_at).max(row.created_at));
                            opts.events
                                .instruction_handled_timed(row, &out.result, fill_ms);
                            if out.reported {
                                opts.events.health_changed(true);
                            } else {
                                // H9: executed (or terminally failed) but the
                                // gateway never received the result — stashed
                                // for re-delivery; surface degraded health and
                                // CONTINUE with the batch instead of breaking.
                                opts.runtime.set_error(Some(format!(
                                    "result for {} undelivered — will re-deliver",
                                    row.cloid
                                )));
                                opts.events.health_changed(false);
                            }
                        }
                        Err(e) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            error!(?e, cloid = %row.cloid, consecutive_failures, "handler failed — not advancing cursor");
                            opts.runtime
                                .set_error(Some(format!("instruction {} failed: {e}", row.cloid)));
                            opts.events.health_changed(false);
                            if consecutive_failures >= FAILURE_ALERT_THRESHOLD {
                                error!(
                                    cloid = %row.cloid, consecutive_failures,
                                    "ALERT: {consecutive_failures} consecutive report failures — gateway reporting stuck (execution NOT re-firing)"
                                );
                                opts.runtime.set_error(Some(format!(
                                    "report stuck: {consecutive_failures} consecutive failures (gateway unreachable?)"
                                )));
                            }
                            failed_at = Some(row.created_at);
                            break;
                        }
                    }
                    if let Ok(mut g) = opts.runtime.queue_pending.lock() {
                        *g = total.saturating_sub(idx + 1);
                    }
                }
                // A signed instruction changed the account (open/close/DCA) —
                // ask the balance loop to refresh NOW so the app's Positions/
                // Account views reflect the new state within a round-trip
                // instead of on the next 5 s tick. `try_send` is non-blocking:
                // a full (depth-1) channel already has a refresh queued, and
                // the ticker is the safety net regardless.
                if handled_cursor.is_some() {
                    let _ = bal_nudge_tx.try_send(());
                }
                last_seen = commit_cursor(last_seen, handled_cursor, failed_at);
                opts.events.batch_settled();
            }
            Err(e) => {
                warn!(?e, "poll failed — will retry next tick");
                opts.runtime.set_error(Some(format!("poll failed: {e}")));
                opts.runtime.set_conn(ConnState::Error);
                opts.events.health_changed(false);
            }
        }
    }
}

/// The paper-mode flag that governs the NEXT instruction: the LIVE
/// shared-runtime flag, falling back to the boot value when the lock is
/// poisoned. The runtime flag is seeded from `DaemonOpts::paper_mode` at
/// registration, so hosts that never touch it keep boot semantics —
/// hosts with a runtime toggle (the desktop app) flip the shared flag
/// and the change applies immediately, no respawn needed.
fn effective_paper_mode(runtime: &SharedHlRuntime, boot_default: bool) -> bool {
    runtime
        .paper_mode
        .lock()
        .map(|g| *g)
        .unwrap_or(boot_default)
}

/// Compute the next poll cursor. The gateway filters with STRICT
/// `created_at > since`, so the cursor must never advance to-or-past an
/// un-handled row's instant. When a row at instant `f` failed, clamp
/// strictly below `f`. Never moves backwards. Pure for unit testing.
fn commit_cursor(
    prev: Option<DateTime<Utc>>,
    handled: Option<DateTime<Utc>>,
    failed_at: Option<DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    let Some(mut candidate) = handled else {
        return prev;
    };
    if let Some(f) = failed_at {
        if candidate >= f {
            candidate = f - chrono::Duration::nanoseconds(1);
        }
    }
    Some(prev.map_or(candidate, |p| p.max(candidate)))
}

async fn balance_refresh_loop(
    network: Network,
    master: String,
    runtime: SharedHlRuntime,
    my_generation: u64,
    mut nudge_rx: tokio::sync::mpsc::Receiver<()>,
) {
    let info = match HttpInfoClient::new(network) {
        Ok(c) => c,
        Err(e) => {
            if let Ok(mut g) = runtime.balance.lock() {
                g.error = Some(format!("balance client init failed: {e}"));
            }
            return;
        }
    };
    let mut ticker = interval(Duration::from_secs(5));
    loop {
        // Fetch on the 5 s ticker OR the moment the poll loop nudges us
        // after signing an instruction — so a fresh balance/position
        // snapshot lands within ~one HL round-trip of a fill instead of
        // waiting out the interval.
        tokio::select! {
            _ = ticker.tick() => {}
            _ = nudge_rx.recv() => {}
        }
        if !runtime.daemon_running.load(Ordering::Relaxed)
            || runtime.run_generation.load(Ordering::SeqCst) != my_generation
        {
            return;
        }
        match info.account_summary(&master).await {
            Ok(summary) => {
                let positions = summary
                    .positions
                    .iter()
                    .map(|p| PositionRow {
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
                    *g = BalanceSnapshot {
                        account_value_usd: summary.account_value_usd,
                        withdrawable_usd: summary.withdrawable_usd,
                        spot_usdc: summary.spot_usdc,
                        is_unified: summary.is_unified,
                        unified_value_usd: summary.unified_value_usd,
                        positions,
                        fetched_at: Some(Utc::now()),
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
}

/// Pull a human asset label off the instruction payload for telemetry.
pub fn payload_asset(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("asset")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("coin").and_then(|v| v.as_str()))
        .map(|s| s.to_string())
}

/// Instruction kind label (`order` when absent) for telemetry.
pub fn payload_kind(payload: &serde_json::Value) -> String {
    payload
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("order")
        .to_string()
}

/// What `handle_one` resolved for a row. `reported = false` means the
/// result was executed (or terminally failed) but the gateway POST kept
/// failing — the result is stashed in the executed store for re-delivery
/// and the batch CONTINUES (H9).
struct HandleOutcome {
    result: SignedSubmitResult,
    reported: bool,
}

/// `true` for execute errors that can NEVER succeed on a retry — a
/// malformed/unknown payload (M14) or a missing master pairing. These
/// must produce a terminal `failed` report so the gateway row finalises
/// honestly instead of reclaim-expiring as "signer offline".
fn is_terminal_sign_error(e: &SignError) -> bool {
    matches!(
        e,
        SignError::Decode(_) | SignError::BadPayload(_) | SignError::MissingAccount
    )
}

async fn handle_one(
    ctx: &ExecContext,
    server: &ServerClient,
    row: &PendingRow,
    audit: Option<&AuditLog>,
    executed: Option<&ExecutedStore>,
    paper_mode: bool,
    runtime: &SharedHlRuntime,
) -> Result<HandleOutcome> {
    // PAPER MODE: real dry-run. Never call execute (which POSTs to HL);
    // report a `paper` status so the gateway row finalises (the gateway
    // accepts `paper` and, when IT is live, fails the order loudly with
    // a config-mismatch message instead of bouncing the report).
    if paper_mode {
        info!(cloid = %row.cloid, "paper mode — NOT submitting to HL");
        let result = SignedSubmitResult {
            cloid: row.cloid.clone(),
            oid: None,
            status: "paper".into(),
            filled_size_usd: None,
            closed_pnl: None,
            err_msg: Some("paper mode — dry run, not submitted to HL".into()),
        };
        let reported = report_or_stash(server, runtime, executed, &row.cloid, &result, None, None)
            .await
            .map_err(|e| anyhow!("post_result (paper): {e}"))?;
        return Ok(HandleOutcome { result, reported });
    }

    // IDEMPOTENCY: if execute already SUCCEEDED on a prior poll but
    // post_result failed, replay the cached result — never re-execute.
    let signed_at = Utc::now();
    let result: SignedSubmitResult = match executed.and_then(|e| e.get(&row.cloid)) {
        Some(cached) => {
            info!(cloid = %row.cloid, status = %cached.status, "already executed — retrying report only");
            cached
        }
        None => {
            debug!(cloid = %row.cloid, "signing + submitting to HL");
            match execute(&row.payload, ctx).await {
                Ok(result) => {
                    // CRITICAL: persist the marker BEFORE post_result so a crash
                    // between execute and report can't re-open the re-submit window.
                    if let Some(store) = executed {
                        if let Err(e) = store.mark_executed(&row.cloid, &result) {
                            warn!(?e, cloid = %row.cloid, "could not persist executed-marker — a retry may re-submit");
                        }
                    }
                    result
                }
                // M14: a payload this signer can never execute (decode error,
                // bad fields, unpaired account) must report a terminal
                // `failed` — bubbling Err would strand the whole batch and
                // the gateway would later mislabel the row "signer offline".
                Err(e) if is_terminal_sign_error(&e) => {
                    error!(
                        cloid = %row.cloid, error = %e,
                        "terminal payload error — reporting failed so the batch is not stranded"
                    );
                    SignedSubmitResult {
                        cloid: row.cloid.clone(),
                        oid: None,
                        status: "failed".into(),
                        filled_size_usd: None,
                        closed_pnl: None,
                        err_msg: Some(format!("bad_payload: {e}")),
                    }
                }
                // Transient (HL info fetch etc.) — nothing executed; hold the
                // cursor and retry the row on the next poll.
                Err(e) => return Err(anyhow!("execute: {e}")),
            }
        }
    };
    let posted_to_hl_at = Utc::now();

    // Local audit line — the user's own record of what their key signed,
    // independent of the server. Best-effort; never blocks the trade.
    if let Some(a) = audit {
        a.record_lossy(&AuditEntry {
            ts: posted_to_hl_at,
            source: "daemon",
            cloid: result.cloid.clone(),
            kind: payload_kind(&row.payload),
            asset: payload_asset(&row.payload),
            status: result.status.clone(),
            oid: result.oid,
            error: result.err_msg.clone(),
        });
    }

    let reported = report_or_stash(
        server,
        runtime,
        executed,
        &row.cloid,
        &result,
        Some(signed_at),
        Some(posted_to_hl_at),
    )
    .await
    .map_err(|e| anyhow!("post_result: {e}"))?;

    if reported {
        info!(cloid = %result.cloid, oid = ?result.oid, status = %result.status, "instruction acked to gateway");
    }
    Ok(HandleOutcome { result, reported })
}

/// Bounded-backoff retry schedule for `post_result` (H9): 5 attempts
/// total, sleeping 250ms/500ms/1s/2s between them (~3.75s worst case per
/// row — bounded so one dead gateway can't hang a protective close batch
/// for long).
const POST_RESULT_ATTEMPTS: u32 = 5;
const POST_RESULT_BACKOFF_MS: [u64; 4] = [250, 500, 1_000, 2_000];

/// POST a result with bounded retries; on persistent failure stash it as
/// undelivered in the executed store and return `Ok(false)` so the batch
/// can continue (the flush pass re-delivers it on later cycles). Only
/// when there is NO store to stash into do we propagate the error — the
/// caller then falls back to the legacy hold-cursor-and-break behaviour
/// (losing the result forever is the one outcome we refuse).
async fn report_or_stash(
    server: &ServerClient,
    runtime: &SharedHlRuntime,
    executed: Option<&ExecutedStore>,
    cloid: &str,
    result: &SignedSubmitResult,
    signed_at: Option<DateTime<Utc>>,
    posted_to_hl_at: Option<DateTime<Utc>>,
) -> Result<bool, ServerError> {
    let req = ResultReq {
        cloid: result.cloid.clone(),
        oid: result.oid,
        status: result.status.clone(),
        filled_size_usd: result.filled_size_usd.clone(),
        closed_pnl: result.closed_pnl.clone(),
        err_msg: result.err_msg.clone(),
        signed_at,
        posted_to_hl_at,
    };
    match post_result_resilient(server, &req, runtime).await {
        Ok(()) => {
            // Clear a prior undelivered marker (re-delivery path).
            if let Some(store) = executed {
                if store.is_undelivered(cloid) {
                    if let Err(e) = store.mark_reported(cloid) {
                        warn!(?e, %cloid, "could not persist reported-tombstone");
                    }
                }
            }
            Ok(true)
        }
        Err(e) => {
            if let Some(store) = executed {
                error!(
                    %cloid, error = %e,
                    "ALERT: result POST failed after {POST_RESULT_ATTEMPTS} attempts — stashing for re-delivery and continuing"
                );
                if let Err(e2) = store.mark_undelivered(cloid, result) {
                    error!(?e2, %cloid, "could not persist undelivered result — it may be lost if the process dies");
                }
                Ok(false)
            } else {
                // No store — cannot stash; let the caller hold the cursor.
                Err(e)
            }
        }
    }
}

/// `post_result_with_totp` with the bounded H9 retry schedule. Retries
/// any transport/server error (a flaky network or a 5xx during a deploy
/// is exactly the case that used to lose results forever).
async fn post_result_resilient(
    server: &ServerClient,
    req: &ResultReq,
    runtime: &SharedHlRuntime,
) -> Result<(), ServerError> {
    let mut last_err: Option<ServerError> = None;
    for attempt in 0..POST_RESULT_ATTEMPTS {
        if attempt > 0 {
            let delay = POST_RESULT_BACKOFF_MS[(attempt as usize - 1).min(3)];
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        match post_result_with_totp(server, req, runtime).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                warn!(
                    cloid = %req.cloid, attempt = attempt + 1,
                    error = %e, "post_result attempt failed"
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("at least one attempt ran"))
}

/// H9: one re-delivery pass over the executed-but-unreported results.
/// Single attempt per cloid per cycle (the poll ticker paces retries);
/// the first failure aborts the pass — the gateway is likely down and
/// hammering the rest is pointless this cycle.
async fn flush_undelivered(server: &ServerClient, store: &ExecutedStore, rt: &SharedHlRuntime) {
    let pending = store.undelivered();
    if pending.is_empty() {
        return;
    }
    info!(
        count = pending.len(),
        "re-delivering stashed instruction results"
    );
    for (cloid, result) in pending {
        let req = ResultReq {
            cloid: result.cloid.clone(),
            oid: result.oid,
            status: result.status.clone(),
            filled_size_usd: result.filled_size_usd.clone(),
            closed_pnl: result.closed_pnl.clone(),
            err_msg: result.err_msg.clone(),
            signed_at: None,
            posted_to_hl_at: None,
        };
        match post_result_with_totp(server, &req, rt).await {
            Ok(()) => {
                info!(%cloid, status = %result.status, "stashed result re-delivered");
                if let Err(e) = store.mark_reported(&cloid) {
                    warn!(?e, %cloid, "could not persist reported-tombstone");
                }
            }
            Err(e) => {
                warn!(?e, %cloid, "re-delivery failed — will retry next cycle");
                break;
            }
        }
    }
}

/// `post_result`, transparently handling an HTTP 428 per-trade-TOTP
/// challenge by parking a prompt for the host UI and waiting (bounded) for
/// the operator's code. On success the original result is re-sent with the
/// bypass header; on timeout/cancel a terminal `failed` ack is sent so the
/// gateway row doesn't dangle — and `Ok(())` is returned so the cursor can
/// advance (execution is idempotent; the trade is not re-fired).
async fn post_result_with_totp(
    server: &ServerClient,
    req: &ResultReq,
    runtime: &SharedHlRuntime,
) -> Result<(), ServerError> {
    match server.post_result(req).await {
        Ok(()) => Ok(()),
        Err(ServerError::Status(code, body)) if code == StatusCode::from_u16(428).unwrap() => {
            let Some(challenge) = ServerClient::parse_totp_challenge(&body) else {
                warn!(cloid = %req.cloid, body = %body, "unexpected 428 from gateway");
                return Err(ServerError::Status(code, body));
            };
            info!(cloid = %req.cloid, challenge_id = %challenge.challenge_id, "gateway requires TOTP — prompting host UI");
            // Park the prompt for the host + clear any stale answer.
            if let Ok(mut g) = runtime.totp_answer.lock() {
                *g = None;
            }
            if let Ok(mut g) = runtime.totp_prompt.lock() {
                *g = Some(TotpPrompt {
                    challenge_id: challenge.challenge_id.clone(),
                    expires_at: challenge.expires_at.clone(),
                });
            }
            let code = wait_for_totp_code(runtime).await;
            // Clear the prompt regardless of outcome.
            if let Ok(mut g) = runtime.totp_prompt.lock() {
                *g = None;
            }
            let mut sent = false;
            if let Some(code) = code {
                match server.verify_totp(&challenge.challenge_id, &code).await {
                    Ok(bypass) => match server.post_result_with_bypass(req, &bypass).await {
                        Ok(()) => sent = true,
                        Err(e) => error!(?e, cloid = %req.cloid, "post_result_with_bypass failed"),
                    },
                    Err(e) => warn!(?e, cloid = %req.cloid, "TOTP verification failed"),
                }
            } else {
                warn!(cloid = %req.cloid, "TOTP prompt timed out / cancelled");
            }
            if !sent {
                let fail = ResultReq {
                    status: "failed".into(),
                    filled_size_usd: None,
                    closed_pnl: None,
                    err_msg: Some("totp_required — operator cancelled or code rejected".into()),
                    ..req.clone()
                };
                if let Err(e) = server.post_result(&fail).await {
                    debug!(?e, "could not send failure ack after 428");
                }
            }
            // Reported (bypass or terminal failure) — cursor may advance.
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Poll the shared runtime for a host-submitted TOTP code, up to a bounded
/// window. Returns the code or `None` on timeout.
async fn wait_for_totp_code(runtime: &SharedHlRuntime) -> Option<String> {
    const TIMEOUT: Duration = Duration::from_secs(90);
    const POLL: Duration = Duration::from_millis(250);
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok(mut g) = runtime.totp_answer.lock() {
            if let Some(code) = g.take() {
                return Some(code);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(POLL).await;
    }
}

fn hostname_fallback() -> Option<String> {
    // POSIX uname() via a separate process. Best-effort — used only
    // for support diagnostics, not security-sensitive.
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn cursor_advances_to_max_handled_when_all_ok() {
        assert_eq!(commit_cursor(None, Some(ts(10)), None), Some(ts(10)));
    }

    #[test]
    fn cursor_holds_when_nothing_handled() {
        let prev = Some(ts(5));
        assert_eq!(commit_cursor(prev, None, Some(ts(6))), prev);
    }

    #[test]
    fn cursor_clamps_below_failed_same_instant_sibling() {
        let next = commit_cursor(None, Some(ts(10)), Some(ts(10))).unwrap();
        assert!(
            next < ts(10),
            "cursor must stay strictly below failed instant"
        );
        assert!(ts(10) > next);
    }

    #[test]
    fn cursor_keeps_success_before_later_failure() {
        assert_eq!(
            commit_cursor(None, Some(ts(10)), Some(ts(12))),
            Some(ts(10))
        );
    }

    #[test]
    fn cursor_never_moves_backwards() {
        let prev = Some(ts(20));
        assert_eq!(commit_cursor(prev, Some(ts(10)), Some(ts(10))), prev);
    }

    // ── ClaimScope: the double-execution / wrong-account safety belt ──

    #[test]
    fn unscoped_claim_executes_everything_legacy() {
        let s = ClaimScope::Unscoped;
        assert!(s.row_in_scope(None));
        assert!(s.row_in_scope(Some("0xanything")));
        assert_eq!(s.poll_wallet(), None);
    }

    #[test]
    fn scoped_claim_never_executes_another_wallets_row() {
        // Safety test (a), client half: even if a gateway hands a
        // wallet-scoped daemon another wallet's instruction (old
        // gateway ignoring `?wallet=`), it is refused — for BOTH the
        // primary (allow_unstamped) and strict secondary variants.
        for allow_unstamped in [true, false] {
            let s = ClaimScope::Scoped {
                wallet: "0xAAA1".into(),
                allow_unstamped,
            };
            assert!(
                !s.row_in_scope(Some("0xbbb2")),
                "foreign row must be refused"
            );
            assert!(s.row_in_scope(Some("0xaaa1")), "own row executes");
            assert!(s.row_in_scope(Some("0xAAA1")), "case-insensitive hex");
            assert_eq!(s.poll_wallet(), Some("0xAAA1"));
        }
    }

    #[test]
    fn unstamped_rows_belong_to_the_primary_scope_only() {
        // Safety test (b), HL half: a legacy row WITHOUT a wallet stamp
        // executes on exactly one daemon — the primary
        // (allow_unstamped=true); every strict secondary refuses it.
        let primary = ClaimScope::Scoped {
            wallet: "0xaaa1".into(),
            allow_unstamped: true,
        };
        let secondary = ClaimScope::Scoped {
            wallet: "0xbbb2".into(),
            allow_unstamped: false,
        };
        let takers = [&primary, &secondary]
            .iter()
            .filter(|s| s.row_in_scope(None))
            .count();
        assert_eq!(takers, 1, "exactly one executor owns unstamped rows");
        assert!(primary.row_in_scope(None));
        assert!(!secondary.row_in_scope(None));
    }

    // ── H11: the paper decision reads the LIVE runtime flag ─────────

    #[test]
    fn paper_mode_flip_applies_without_respawn() {
        use crate::hl::runtime::HlRuntime;
        let rt: SharedHlRuntime = Arc::new(HlRuntime::default());

        // Boot seeding semantics: the daemon writes opts.paper_mode into
        // the runtime at registration — mirror that here.
        if let Ok(mut g) = rt.paper_mode.lock() {
            *g = false;
        }
        assert!(
            !effective_paper_mode(&rt, false),
            "live daemon starts in live mode"
        );

        // Host toggles paper ON at runtime → the very next instruction
        // must be a dry-run, with NO respawn.
        if let Ok(mut g) = rt.paper_mode.lock() {
            *g = true;
        }
        assert!(effective_paper_mode(&rt, false), "flip applies live");

        // …and back OFF.
        if let Ok(mut g) = rt.paper_mode.lock() {
            *g = false;
        }
        assert!(
            !effective_paper_mode(&rt, true),
            "runtime flag wins over boot value"
        );
    }

    #[test]
    fn payload_helpers_read_asset_and_kind() {
        let v = serde_json::json!({"kind": "closePosition", "asset": "BTC"});
        assert_eq!(payload_kind(&v), "closePosition");
        assert_eq!(payload_asset(&v).as_deref(), Some("BTC"));
        // `coin` fallback + default kind.
        let w = serde_json::json!({"coin": "ETH"});
        assert_eq!(payload_kind(&w), "order");
        assert_eq!(payload_asset(&w).as_deref(), Some("ETH"));
    }
}
