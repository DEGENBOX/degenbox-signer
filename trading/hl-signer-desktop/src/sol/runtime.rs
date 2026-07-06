//! Solana execution runtime — makes the unified CLI a full Solana
//! executor, mirroring the Tauri app's composition (which itself is a
//! 1:1 port of `signer-cli watch-sells` + `watch-copy`), folded into
//! ONE task with ONE `BotEngine`:
//!
//! ```text
//! unlock → resolve gateway auth → /auth/me (user uuid)
//!        → spawn_sell_subscriber  (trading.sell.needed.{user})
//!        → spawn_copy_subscriber  (trading.copy.exec.{user})
//!        → select! loop → BotEngine::execute_sell / execute_buy
//!          (route dispatch → allowlist → simulate → sign → relay)
//! ```
//!
//! Invariants kept from the CLI/app pair:
//!
//! - **Budget guard stays mandatory.** `signer-cli watch-copy` makes
//!   `--session-sol` required; here the equivalent is
//!   `SolConfig.copy_session_sol` (or the `--session-sol` flag). While
//!   unset, copy BUYS are refused per event (logged + activity entry +
//!   status flag). Sells (TP/SL + mirror) only dispose of tokens and
//!   run regardless — exactly like `watch-sells` which has no budget.
//! - All buys/sells go exclusively through `BotEngine::execute_buy` /
//!   `execute_sell` (allowlist → simulate → sign → relay). No new
//!   signing or send paths.
//! - Per-event failures log + continue — a stale TP must never nuke
//!   the loop.
//! - Restart-safe spawn: a prior loop is signalled to stop and the new
//!   task WAITS (bounded 60 s) for it to release the running guard, so
//!   two loops can never race the same user-scoped subjects.

use crate::sol::config::SolConfig;
use chrono::Utc;
use degenbox_signer_core::{
    bot_engine::Decision, default_allowlist, resolve_buy_lamports, spawn_copy_subscriber,
    spawn_sell_subscriber, wallet_event_is_mine, BotConfig, BotEngine, BudgetConfig, BudgetState,
    CopyExecEvent, JupiterClient, Keypair, PresetMatcher, RelayClient, RpcClient, SellNeededEvent,
    Signer as _, TriggerKind,
};
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// Where the runtime gets its gateway credentials.
#[derive(Clone)]
pub enum AuthSource {
    /// Explicit JWT (e.g. `--token-file`) + gateway base URL.
    Token { base: String, token: String },
    /// Resolve like the desktop app: (1) the Discord desktop login
    /// (`desktop-auth.json`, minted by `hl-signer-desktop login` and
    /// shared with the Tauri app), then (2) the pairing JWT in the
    /// shared `hl-config.json` (`api_token`, minted by
    /// redeem-registration as a normal user JWT), then (3) the session
    /// token the DegenBox web app pushed to the `:5829` daemon via
    /// `POST /setAuth`.
    Auto {
        /// The `:5829` daemon's live runtime config, when it's serving.
        web: Option<Arc<tokio::sync::RwLock<degenbox_signer_core::local_daemon::RuntimeConfig>>>,
    },
}

#[derive(Debug, Clone)]
pub struct GatewayAuth {
    pub base: String,
    pub token: String,
}

/// Resolve gateway credentials, or a user-actionable error.
async fn resolve_auth(source: &AuthSource) -> Result<GatewayAuth, String> {
    match source {
        AuthSource::Token { base, token } => Ok(GatewayAuth {
            base: base.trim_end_matches('/').to_string(),
            token: token.clone(),
        }),
        AuthSource::Auto { web } => {
            if let Some(a) = crate::auth::DesktopAuth::load_valid() {
                return Ok(GatewayAuth {
                    base: a.gateway_base.trim_end_matches('/').to_string(),
                    token: a.token,
                });
            }
            let cfg = crate::config::default_config_path()
                .ok()
                .and_then(|p| crate::config::load(&p).ok())
                .unwrap_or_default();
            if let Some(token) = cfg.api_token.clone() {
                return Ok(GatewayAuth {
                    base: cfg.server_url.trim_end_matches('/').to_string(),
                    token,
                });
            }
            if let Some(rc) = web {
                let g = rc.read().await;
                if let Some(token) = g.auth_token.clone() {
                    return Ok(GatewayAuth {
                        base: g.gateway_base.trim_end_matches('/').to_string(),
                        token,
                    });
                }
            }
            Err(
                "not connected to DegenBox — run `hl-signer-desktop login` (Discord), \
                 `hl-signer-desktop register` (HL pairing JWT works for Solana too), pass \
                 --token-file, or open the DegenBox web app once so it can hand this client a \
                 session token via the local daemon"
                    .into(),
            )
        }
    }
}

/// Live telemetry the TUI panel / status surfaces poll.
#[derive(Debug, Clone, Serialize)]
pub struct SolRuntimeStatus {
    /// "offline" | "waiting_auth" | "connecting" | "ready" | "error"
    pub state: String,
    pub user_id: Option<String>,
    /// True when a copy session budget is configured (copy buys armed).
    pub copy_armed: bool,
    pub copy_session_sol: Option<f64>,
    pub copy_spent_sol: f64,
    pub sells_executed: u64,
    pub copies_executed: u64,
    pub events_failed: u64,
    pub last_event_at: Option<String>,
    pub error: Option<String>,
}

impl Default for SolRuntimeStatus {
    fn default() -> Self {
        Self {
            state: "offline".into(),
            user_id: None,
            copy_armed: false,
            copy_session_sol: None,
            copy_spent_sol: 0.0,
            sells_executed: 0,
            copies_executed: 0,
            events_failed: 0,
            last_event_at: None,
            error: None,
        }
    }
}

/// One executed/skipped/failed Solana event for the activity feed.
#[derive(Debug, Clone)]
pub struct SolActivity {
    pub at: chrono::DateTime<chrono::Utc>,
    /// "TP sell" | "SL sell" | "copy buy" | "copy sell" | …
    pub kind: String,
    pub mint: String,
    /// "submitted" | "skipped" | "failed"
    pub status: String,
}

#[derive(Default)]
pub struct SolRuntimeInner {
    pub status: Mutex<SolRuntimeStatus>,
    /// Last 50 handled events, newest at the back.
    pub activity: Mutex<VecDeque<SolActivity>>,
    /// Send `true` to stop the loop after the in-flight event finishes.
    stop: Mutex<Option<watch::Sender<bool>>>,
    running: AtomicBool,
}

pub type SharedSolRuntime = Arc<SolRuntimeInner>;

impl SolRuntimeInner {
    pub fn snapshot(&self) -> SolRuntimeStatus {
        self.status.lock().map(|g| g.clone()).unwrap_or_default()
    }

    fn update(&self, f: impl FnOnce(&mut SolRuntimeStatus)) {
        if let Ok(mut g) = self.status.lock() {
            f(&mut g);
        }
    }

    fn push_activity(&self, kind: &str, mint: &str, status: &str) {
        if let Ok(mut g) = self.activity.lock() {
            if g.len() >= 50 {
                g.pop_front();
            }
            g.push_back(SolActivity {
                at: Utc::now(),
                kind: kind.to_string(),
                mint: mint.to_string(),
                status: status.to_string(),
            });
        }
    }

    /// Stop the runtime (lock / quit / config change). Idempotent.
    pub fn stop(&self) {
        if let Ok(mut g) = self.stop.lock() {
            if let Some(tx) = g.take() {
                let _ = tx.send(true);
            }
        }
        self.update(|s| {
            s.state = "offline".into();
            s.error = None;
        });
    }
}

/// Everything `run` needs beyond the shared handle.
pub struct SpawnArgs {
    pub kp: Arc<Keypair>,
    pub auth: AuthSource,
    pub cfg: SolConfig,
    /// Print per-event outcomes to stdout (headless `sol daemon`); the
    /// TUI reads the activity ring instead.
    pub stdout_log: bool,
}

/// Spawn the Solana runtime. Idempotent + restart-safe: an existing
/// loop is signalled to stop (replacing its stop sender), and the new
/// task WAITS (bounded) for it to release the `running` guard before
/// taking over — so quick relock/budget-change restarts never leave the
/// runtime dead, and two loops can never race the same subjects.
pub fn spawn(rt: SharedSolRuntime, args: SpawnArgs) {
    let (stop_tx, stop_rx) = watch::channel(false);
    if let Ok(mut g) = rt.stop.lock() {
        if let Some(old) = g.replace(stop_tx) {
            let _ = old.send(true);
        }
    }
    let rt2 = rt.clone();
    tokio::spawn(async move {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if rt2
                .running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
            if *stop_rx.borrow() {
                return;
            }
            if std::time::Instant::now() > deadline {
                tracing::error!(
                    "sol runtime: prior loop did not release within 60 s — not starting"
                );
                rt2.update(|s| {
                    s.state = "error".into();
                    s.error = Some("previous Solana runtime did not stop in time — retry".into());
                });
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let res = run(rt2.clone(), args, stop_rx).await;
        if let Err(e) = &res {
            tracing::error!(error = %e, "sol runtime exited with error");
            rt2.update(|s| {
                s.state = "error".into();
                s.error = Some(e.clone());
            });
        } else {
            rt2.update(|s| s.state = "offline".into());
        }
        rt2.running.store(false, Ordering::SeqCst);
    });
}

/// Wait `secs`, returning `true` when stop was requested meanwhile.
async fn stopped_or_sleep(stop_rx: &mut watch::Receiver<bool>, secs: u64) -> bool {
    tokio::select! {
        _ = stop_rx.changed() => *stop_rx.borrow(),
        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => false,
    }
}

async fn run(
    rt: SharedSolRuntime,
    args: SpawnArgs,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let cfg = args.cfg;
    let copy_armed = cfg
        .copy_session_sol
        .is_some_and(|s| s.is_finite() && s > 0.0);
    rt.update(|s| {
        *s = SolRuntimeStatus {
            state: "waiting_auth".into(),
            copy_armed,
            copy_session_sol: cfg.copy_session_sol,
            ..Default::default()
        };
    });

    // 1. Gateway credentials. Retry forever (15 s) — the web app may
    //    push a session token to the :5829 daemon at any time. Status
    //    surfaces the wait so it is never a silent dead state.
    let auth: GatewayAuth = loop {
        if *stop_rx.borrow() {
            return Ok(());
        }
        match resolve_auth(&args.auth).await {
            Ok(a) => break a,
            Err(e) => {
                rt.update(|s| {
                    s.state = "waiting_auth".into();
                    s.error = Some(e);
                });
                if stopped_or_sleep(&mut stop_rx, 15).await {
                    return Ok(());
                }
            }
        }
    };
    rt.update(|s| {
        s.state = "connecting".into();
        s.error = None;
    });

    // 2. Resolve our user uuid — the WS multiplexer rejects wildcards
    //    on both subjects, so we must know it before subscribing.
    let relay_init = RelayClient::new(auth.base.clone(), auth.token.clone());
    let user_id = loop {
        if *stop_rx.borrow() {
            return Ok(());
        }
        match relay_init.fetch_user_id().await {
            Ok(id) => break id,
            Err(e) => {
                rt.update(|s| {
                    s.error = Some(format!("auth/me failed: {e}"));
                });
                if stopped_or_sleep(&mut stop_rx, 15).await {
                    return Ok(());
                }
            }
        }
    };
    rt.update(|s| {
        s.user_id = Some(user_id.to_string());
        s.error = None;
    });

    // 3. Subscribe to both user-scoped streams. The subscribers own
    //    their reconnect loops; dropping the receivers stops them.
    let mut sell_rx = spawn_sell_subscriber(auth.base.clone(), auth.token.clone(), user_id)
        .await
        .map_err(|e| format!("sell stream subscribe: {e}"))?;
    let mut copy_rx = spawn_copy_subscriber(auth.base.clone(), auth.token.clone(), user_id)
        .await
        .map_err(|e| format!("copy stream subscribe: {e}"))?;

    // 4. One engine for both event kinds — sells bypass matcher/budget
    //    by construction (`execute_sell`), so the budget here guards
    //    exactly the copy buys, like watch-copy's BudgetState.
    let matcher = PresetMatcher {
        min_mcap_usd: None,
        max_mcap_usd: None,
        min_liquidity_usd: None,
        max_age_secs: None,
        blocked_tokens: Default::default(),
    };
    let session_lamports = cfg
        .copy_session_sol
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| (s * 1e9) as u64)
        // Disarmed: zero budget. Copy buys are pre-refused before the
        // engine, so this is a second fence, not the primary gate.
        .unwrap_or(0);
    let budget = BudgetState::new(BudgetConfig {
        session_budget_lamports: session_lamports,
        per_token_cap_lamports: cfg
            .copy_per_token_sol
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| (s * 1e9) as u64),
        per_hour_cap_lamports: None,
    });
    let allowlist = default_allowlist().map_err(|e| format!("allowlist: {e}"))?;
    let rpc_url = cfg.resolved_rpc_url();
    let bot_cfg = BotConfig {
        per_trade_lamports: 0, // per-event via execute_buy
        slippage_bps: cfg.slippage_bps,
        tip_lamports: cfg.tip_lamports,
        submit_mode: cfg.submit_mode.clone(),
        rpc_url: rpc_url.clone(),
        skip_simulate: false,
        skip_allowlist: false,
        input_mint: "So11111111111111111111111111111111111111112".into(),
        pumpfun_cu_limit: 120_000,
        pumpfun_cu_price_micro_lamports: 50_000,
        bot_session_id: None,
        preset_id: None,
    };
    let mut engine = BotEngine::new(matcher, budget, allowlist, bot_cfg);
    let jup = JupiterClient::new();
    let relay = RelayClient::new(auth.base.clone(), auth.token.clone());
    let rpc = RpcClient::new(rpc_url);

    rt.update(|s| s.state = "ready".into());
    tracing::info!(%user_id, copy_armed, "sol runtime ready (sell + copy streams live)");
    if args.stdout_log {
        println!(
            "→ Solana streams live (user {user_id}, copy buys {})",
            if copy_armed {
                "ARMED"
            } else {
                "disarmed — set a session budget"
            }
        );
    }

    // 5. Consume events until stop. Per-event failures log + continue.
    //    EXECUTOR ROUTING (multi-client gateways): both streams may
    //    stamp an executor `wallet_pubkey`. This engine runs the
    //    designated PRIMARY wallet, so it executes unstamped (legacy)
    //    events plus events stamped for exactly its own pubkey —
    //    `wallet_event_is_mine` — and skips the rest (they belong to a
    //    different wallet's engine, possibly on another device).
    let my_pubkey = args.kp.pubkey().to_string();
    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    tracing::info!("sol runtime stopping");
                    return Ok(());
                }
            }
            evt = sell_rx.recv() => {
                let Some(evt) = evt else {
                    return Err("sell stream ended — restart to reconnect".into());
                };
                if !wallet_event_is_mine(evt.wallet_pubkey.as_deref(), &my_pubkey, true) {
                    tracing::info!(mint = %short(&evt.mint), executor = ?evt.wallet_pubkey,
                        "sell event stamped for another wallet — skipped");
                    rt.push_activity("sell", &evt.mint, "skipped (other wallet)");
                    continue;
                }
                handle_sell(&rt, args.stdout_log, &mut engine, &jup, &relay, &rpc, args.kp.as_ref(), evt).await;
            }
            evt = copy_rx.recv() => {
                let Some(evt) = evt else {
                    return Err("copy stream ended — restart to reconnect".into());
                };
                if !wallet_event_is_mine(evt.wallet_pubkey.as_deref(), &my_pubkey, true) {
                    tracing::info!(mint = %short(&evt.mint), executor = ?evt.wallet_pubkey,
                        "copy event stamped for another wallet — skipped");
                    rt.push_activity("copy", &evt.mint, "skipped (other wallet)");
                    continue;
                }
                handle_copy(&rt, args.stdout_log, copy_armed, &mut engine, &jup, &relay, &rpc, args.kp.as_ref(), evt).await;
            }
        }
    }
}

fn short(addr: &str) -> String {
    if addr.len() <= 10 {
        addr.to_string()
    } else {
        format!("{}…{}", &addr[..4], &addr[addr.len() - 4..])
    }
}

fn mark_event(rt: &SharedSolRuntime, ok: bool, sell: bool) {
    rt.update(|s| {
        s.last_event_at = Some(Utc::now().to_rfc3339());
        if ok {
            if sell {
                s.sells_executed += 1;
            } else {
                s.copies_executed += 1;
            }
        } else {
            s.events_failed += 1;
        }
    });
}

fn outcome_line(stdout_log: bool, kind: &str, mint: &str, status: &str) {
    if stdout_log {
        println!("· {kind:9} {} → {status}", short(mint));
    }
}

/// TP/SL sell trigger — port of `watch_sells_cmd`'s event body.
#[allow(clippy::too_many_arguments)]
async fn handle_sell(
    rt: &SharedSolRuntime,
    stdout_log: bool,
    engine: &mut BotEngine,
    jup: &JupiterClient,
    relay: &RelayClient,
    rpc: &RpcClient,
    kp: &Keypair,
    evt: SellNeededEvent,
) {
    let kind_label = match evt.trigger_kind {
        TriggerKind::Tp => "TP sell",
        TriggerKind::Sl => "SL sell",
    };
    let token_amount: u64 = match evt.token_amount_raw.parse() {
        Ok(n) => n,
        Err(_) => {
            tracing::warn!(mint = %evt.mint, raw = %evt.token_amount_raw,
                "sell event amount not parseable as u64 — skipped");
            rt.push_activity(kind_label, &evt.mint, "skipped");
            mark_event(rt, false, true);
            return;
        }
    };
    tracing::info!(mint = %short(&evt.mint), amount = token_amount, kind = kind_label,
        price = %evt.triggered_at_price_usd, "sell trigger received");
    match engine
        .execute_sell(
            evt.mint.clone(),
            token_amount,
            evt.amm_address.clone(),
            jup,
            relay,
            rpc,
            kp,
        )
        .await
    {
        Ok(Decision::Submitted(_)) => {
            rt.push_activity(kind_label, &evt.mint, "submitted");
            outcome_line(stdout_log, kind_label, &evt.mint, "submitted");
            mark_event(rt, true, true);
        }
        Ok(Decision::Skipped(r)) => {
            tracing::info!(mint = %short(&evt.mint), reason = %r, "sell skipped");
            rt.push_activity(kind_label, &evt.mint, "skipped");
            outcome_line(stdout_log, kind_label, &evt.mint, "skipped");
            mark_event(rt, false, true);
        }
        Err(e) => {
            tracing::warn!(mint = %short(&evt.mint), error = %e, "sell failed");
            rt.push_activity(kind_label, &evt.mint, "failed");
            outcome_line(stdout_log, kind_label, &evt.mint, "failed");
            rt.update(|s| s.error = Some(format!("{kind_label} {}: {e}", short(&evt.mint))));
            mark_event(rt, false, true);
        }
    }
}

/// Copy-trade execution command — port of `watch_copy_cmd`'s event body.
#[allow(clippy::too_many_arguments)]
async fn handle_copy(
    rt: &SharedSolRuntime,
    stdout_log: bool,
    copy_armed: bool,
    engine: &mut BotEngine,
    jup: &JupiterClient,
    relay: &RelayClient,
    rpc: &RpcClient,
    kp: &Keypair,
    evt: CopyExecEvent,
) {
    // Tag every intent this event produces with its copy config so the
    // gateway can auto-arm the ladder on fill + enforce the position cap.
    engine.set_copy_config_id(Some(evt.config_id.to_string()));
    let slippage = u16::try_from(evt.slippage_bps.clamp(1, 10_000)).unwrap_or(100);
    match evt.side.as_str() {
        "buy" => {
            // MANDATORY budget guard (watch-copy parity): no configured
            // session budget → refuse, loudly.
            if !copy_armed {
                tracing::warn!(mint = %short(&evt.mint),
                    "copy buy refused — no session budget configured (sol budget / --session-sol)");
                rt.push_activity("copy buy", &evt.mint, "skipped");
                outcome_line(stdout_log, "copy buy", &evt.mint, "REFUSED (no budget)");
                rt.update(|s| {
                    s.error = Some(
                        "copy buy refused — set a session budget (Solana tab [b], `sol daemon \
                         --session-sol`, or sol-config.json)"
                            .into(),
                    );
                });
                mark_event(rt, false, false);
                engine.set_copy_config_id(None);
                return;
            }
            // Resolve size: fixed mode arrives pre-sized; pct mode reads
            // the live balance. The cap clamp applies to both.
            let balance = if evt.sizing_mode == 1 {
                match rpc.get_balance(&kp.pubkey()).await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(mint = %short(&evt.mint), error = %e,
                            "copy buy skipped — balance fetch failed");
                        rt.push_activity("copy buy", &evt.mint, "failed");
                        mark_event(rt, false, false);
                        engine.set_copy_config_id(None);
                        return;
                    }
                }
            } else {
                0
            };
            let lamports = match resolve_buy_lamports(&evt, balance) {
                Ok(l) => l,
                Err(reason) => {
                    tracing::info!(mint = %short(&evt.mint), %reason, "copy buy skipped");
                    rt.push_activity("copy buy", &evt.mint, "skipped");
                    outcome_line(stdout_log, "copy buy", &evt.mint, "skipped");
                    mark_event(rt, false, false);
                    engine.set_copy_config_id(None);
                    return;
                }
            };
            tracing::info!(mint = %short(&evt.mint), sol = lamports as f64 / 1e9,
                wallet = %short(&evt.wallet_address), "copy buy executing");
            match engine
                .execute_buy(
                    evt.intent_id.to_string(),
                    evt.mint.clone(),
                    lamports,
                    Some(slippage),
                    evt.amm_address.clone(),
                    jup,
                    relay,
                    rpc,
                    kp,
                )
                .await
            {
                Ok(Decision::Submitted(_)) => {
                    rt.push_activity("copy buy", &evt.mint, "submitted");
                    outcome_line(stdout_log, "copy buy", &evt.mint, "submitted");
                    mark_event(rt, true, false);
                }
                Ok(Decision::Skipped(r)) => {
                    tracing::info!(mint = %short(&evt.mint), reason = %r, "copy buy skipped");
                    rt.push_activity("copy buy", &evt.mint, "skipped");
                    outcome_line(stdout_log, "copy buy", &evt.mint, "skipped");
                    mark_event(rt, false, false);
                }
                Err(e) => {
                    tracing::warn!(mint = %short(&evt.mint), error = %e, "copy buy failed");
                    rt.push_activity("copy buy", &evt.mint, "failed");
                    outcome_line(stdout_log, "copy buy", &evt.mint, "failed");
                    rt.update(|s| s.error = Some(format!("copy buy {}: {e}", short(&evt.mint))));
                    mark_event(rt, false, false);
                }
            }
            // Track client-side spend for the status surfaces.
            let stats = engine.stats();
            let spent = session_spent_sol(stats.budget_remaining_lamports, rt);
            rt.update(|s| s.copy_spent_sol = spent);
        }
        "sell" => {
            let Some(raw) = evt.token_amount_raw.as_deref() else {
                tracing::warn!(mint = %short(&evt.mint), "mirror sell without amount — skipped");
                rt.push_activity("copy sell", &evt.mint, "skipped");
                mark_event(rt, false, true);
                engine.set_copy_config_id(None);
                return;
            };
            let token_amount: u64 = match raw.parse() {
                Ok(n) => n,
                Err(_) => {
                    tracing::warn!(mint = %short(&evt.mint), raw,
                        "mirror sell amount not parseable as u64 — skipped");
                    rt.push_activity("copy sell", &evt.mint, "skipped");
                    mark_event(rt, false, true);
                    engine.set_copy_config_id(None);
                    return;
                }
            };
            tracing::info!(mint = %short(&evt.mint), amount = token_amount,
                wallet = %short(&evt.wallet_address), "mirror sell executing");
            match engine
                .execute_sell(
                    evt.mint.clone(),
                    token_amount,
                    evt.amm_address.clone(),
                    jup,
                    relay,
                    rpc,
                    kp,
                )
                .await
            {
                Ok(Decision::Submitted(_)) => {
                    rt.push_activity("copy sell", &evt.mint, "submitted");
                    outcome_line(stdout_log, "copy sell", &evt.mint, "submitted");
                    mark_event(rt, true, true);
                }
                Ok(Decision::Skipped(r)) => {
                    tracing::info!(mint = %short(&evt.mint), reason = %r, "mirror sell skipped");
                    rt.push_activity("copy sell", &evt.mint, "skipped");
                    outcome_line(stdout_log, "copy sell", &evt.mint, "skipped");
                    mark_event(rt, false, true);
                }
                Err(e) => {
                    tracing::warn!(mint = %short(&evt.mint), error = %e, "mirror sell failed");
                    rt.push_activity("copy sell", &evt.mint, "failed");
                    outcome_line(stdout_log, "copy sell", &evt.mint, "failed");
                    rt.update(|s| s.error = Some(format!("copy sell {}: {e}", short(&evt.mint))));
                    mark_event(rt, false, true);
                }
            }
        }
        other => {
            tracing::warn!(side = %other, mint = %short(&evt.mint), "unknown copy event side");
            rt.push_activity("copy ?", &evt.mint, "skipped");
            mark_event(rt, false, false);
        }
    }
    engine.set_copy_config_id(None);
}

fn session_spent_sol(budget_remaining_lamports: u64, rt: &SharedSolRuntime) -> f64 {
    let session = rt
        .snapshot()
        .copy_session_sol
        .filter(|s| s.is_finite() && *s > 0.0)
        .unwrap_or(0.0);
    (session - budget_remaining_lamports as f64 / 1e9).max(0.0)
}
