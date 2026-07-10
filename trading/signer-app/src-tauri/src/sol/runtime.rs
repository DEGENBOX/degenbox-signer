//! Solana execution runtime — makes the Tauri app a full Solana
//! executor, the same way `signer-cli watch-sells` is.
//!
//! Multi-client topology: ONE dispatcher task per device consuming the
//! two user-scoped streams ONCE, fanned out to N per-wallet
//! [`BotEngine`]s (one per unlocked Sol vault wallet):
//!
//! ```text
//! unlock → resolve gateway auth → /auth/me (user uuid)
//!        → spawn_sell_subscriber   (trading.sell.needed.{user})  1 WS
//!        → spawn_intent_subscriber (trading.intent.{user})       1 WS
//!        → select! loop
//!            → route event by `wallet_pubkey`
//!              (stamped → exactly the matching engine,
//!               unstamped/legacy → the PRIMARY engine only)
//!            → per-client pause gate
//!            → engine[i].execute_sell / execute_buy with keypair[i]
//! ```
//!
//! The single consumption point is the double-execution guarantee: an
//! event is received once and routed to at most one engine — N engines
//! never share a subject subscription. (2 websockets per device total,
//! independent of wallet count.) Manual intents (`trading.intent`) carry
//! no wallet stamp, so they route to the PRIMARY engine and reuse the
//! EXISTING `trading_intents` row (submit-to-existing, never a second).
//!
//! Solana copy-trade (mirroring a leader wallet via `trading.copy.exec`)
//! was removed — that venue is speed-critical and served by native bots
//! elsewhere. Scanner preset auto-buys, manual dashboard intents and
//! TP/SL sells stay.
//!
//! Differences from the CLI, all host-shape:
//!
//! - The engines' `BudgetState` stays as a plain spend LEDGER (unlimited
//!   caps) so the Status page still shows per-session spend. Sells
//!   (TP/SL) only dispose of tokens and were never budget-gated —
//!   exactly like `watch-sells`.
//! - Starts only after keystore unlock; stops via a watch channel on
//!   lock/quit — in-flight event handling completes before the loop
//!   exits.
//! - Outcomes land in the recent-signs ring (Activity page) and the
//!   per-wallet status structs (the primary's doubles as the legacy
//!   device-level `sol_runtime_status`).

use crate::sol::config::SolConfig;
use crate::sol::gateway::{self, GatewayAuth};
use crate::state::{AppState, ClientRole, RecentSign};
use chrono::Utc;
use degenbox_signer_core::{
    bot_engine::Decision, default_allowlist, spawn_intent_subscriber_with,
    spawn_sell_subscriber_with, wallet_event_is_mine, BotConfig, BotEngine, BudgetConfig,
    BudgetState, JupiterClient, Keypair, ManualIntentEvent, PresetMatcher, RelayClient, RelayError,
    RpcClient, SellNeededEvent, Signer as _, StreamAuth, StreamHealth, StreamHealthSink,
    TokenProvider, TriggerKind, WalletChain,
};
use serde::Serialize;
use solana_sdk::signature::SeedDerivable;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::Manager;
use tokio::sync::watch;

/// Live telemetry the Status page polls via `sol_runtime_status`.
#[derive(Debug, Clone, Serialize)]
pub struct SolRuntimeStatus {
    /// "offline" | "waiting_auth" | "connecting" | "ready" |
    /// "auth_expired" | "error"
    ///
    /// `auth_expired` = the gateway rejected our credentials (401/403)
    /// or none resolve any more — execution is DOWN until the user
    /// re-logs in (account menu, top right) or another credential appears. Distinct
    /// from `waiting_auth` (never had credentials) so the UI can say
    /// "Re-login required" instead of a generic wait.
    pub state: String,
    pub user_id: Option<String>,
    /// Copy buys armed. Since slice 8 this is always true while the
    /// runtime runs — budgets are per-config server-side; the field
    /// stays for UI wire-compat until the slice-9 redesign binds.
    pub copy_armed: bool,
    /// RETIRED (slice 8): the per-unlock client budget. Always `None`.
    pub copy_session_sol: Option<f64>,
    /// SOL spent on copy buys by this runtime since unlock (ledger).
    pub copy_spent_sol: f64,
    pub sells_executed: u64,
    pub copies_executed: u64,
    pub events_failed: u64,
    pub last_event_at: Option<String>,
    /// Engine liveness heartbeat — stamped when the dispatcher goes
    /// ready, on every handled event and every 30 s while the select
    /// loop is alive. `None` = the engine is not running (or predates
    /// this field). Drives the status line's "Heartbeat" (slice 9 §A).
    pub alive_at: Option<String>,
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
            alive_at: None,
            error: None,
        }
    }
}

#[derive(Default)]
pub struct SolRuntimeInner {
    pub status: Mutex<SolRuntimeStatus>,
    /// Send `true` to stop the loop after the in-flight event finishes.
    pub stop: Mutex<Option<watch::Sender<bool>>>,
    pub running: AtomicBool,
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
}

/// Stop the runtime (lock / quit / config change). Idempotent.
pub fn stop(state: &AppState) {
    if let Ok(mut g) = state.sol_runtime.stop.lock() {
        if let Some(tx) = g.take() {
            let _ = tx.send(true);
        }
    }
    state.sol_runtime.update(|s| {
        s.state = "offline".into();
        s.error = None;
    });
}

/// One per-wallet executor inside the dispatcher: identity + keypair +
/// pause gate + telemetry sink. The matching `BotEngine` lives in the
/// run loop (parallel `Vec`, same index).
pub struct EngineSlot {
    /// Wallet pubkey (base58) — the routing key for stamped events.
    pub wallet: String,
    /// Owns legacy (unstamped) events; mirrors into the device-level
    /// status.
    pub primary: bool,
    pub kp: Arc<Keypair>,
    /// Effective pause gate (global kill-switch OR per-client pause),
    /// shared with `clients::recompute_pause_gates` — read PER EVENT so
    /// pausing one client never touches its siblings.
    pub pause_gate: Arc<Mutex<bool>>,
    pub rt: SharedSolRuntime,
}

/// Build the engine slots from the unlocked vault clients; fall back to
/// the legacy single-wallet slot (`:5829` SignerSlot keypair) for
/// pre-vault installs.
fn build_slots(state: &AppState) -> Vec<EngineSlot> {
    let mut slots: Vec<EngineSlot> = Vec::new();
    if let Ok(clients) = state.clients.lock() {
        for c in clients.iter() {
            if c.entry.chain != WalletChain::Sol {
                continue;
            }
            let Some(seed) = &c.sol_seed else { continue };
            let Ok(kp) = Keypair::from_seed(seed) else {
                tracing::error!(wallet = %c.entry.address, "sol seed did not derive a keypair — slot skipped");
                continue;
            };
            slots.push(EngineSlot {
                wallet: c.entry.address.clone(),
                primary: c.role == ClientRole::Primary,
                kp: Arc::new(kp),
                pause_gate: c.pause_gate.clone(),
                rt: c
                    .sol_runtime
                    .clone()
                    .unwrap_or_else(|| state.sol_runtime.clone()),
            });
        }
    }
    if slots.is_empty() {
        // Legacy pre-vault install: one wallet straight from the :5829
        // signer slot, gated by the global kill-switch only.
        if let Some(kp) = state.sol_slot.unlocked() {
            slots.push(EngineSlot {
                wallet: kp.pubkey().to_string(),
                primary: true,
                kp,
                pause_gate: state.paused.clone(),
                rt: state.sol_runtime.clone(),
            });
        }
    }
    slots
}

/// Spawn the Solana dispatcher if at least one Sol wallet is unlocked.
/// Idempotent + restart-safe: an existing loop is signalled to stop
/// (replacing its stop sender), and the new task WAITS for it to
/// release the `running` guard before taking over — so quick
/// lock→unlock cycles and budget-change restarts never leave the
/// runtime dead, and two loops can never race the same user-scoped
/// subjects (double-execution).
pub fn spawn(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    // Global kill-switch: no dispatcher at all while paused (the
    // per-client gates additionally guard each engine inline).
    let globally_paused = state.paused.lock().map(|g| *g).unwrap_or(false);
    if globally_paused {
        stop(&state);
        state.sol_runtime.update(|s| {
            s.state = "offline".into();
            s.error = Some("paused — resume to restart the Solana engine".into());
        });
        return;
    }
    let slots = build_slots(&state);
    if slots.is_empty() {
        // No Solana wallet unlocked — nothing to run. Not an error:
        // HL-only installs are valid.
        state.sol_runtime.update(|s| {
            s.state = "offline".into();
        });
        return;
    }
    let (stop_tx, stop_rx) = watch::channel(false);
    if let Ok(mut g) = state.sol_runtime.stop.lock() {
        // Signal any prior loop to wind down — it finishes its
        // in-flight event, then releases `running`.
        if let Some(old) = g.replace(stop_tx) {
            let _ = old.send(true);
        }
    }
    let rt = state.sol_runtime.clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn(async move {
        // Take the running guard, waiting (bounded) for a prior loop.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            if rt
                .running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
            if *stop_rx.borrow() {
                // We were stopped (lock/another restart) before even
                // starting — bow out quietly.
                return;
            }
            if std::time::Instant::now() > deadline {
                tracing::error!(
                    "sol runtime: prior loop did not release within 60 s — not starting"
                );
                rt.update(|s| {
                    s.state = "error".into();
                    s.error = Some(
                        "previous Solana runtime did not stop in time — \
                         lock and unlock the keystore to restart"
                            .into(),
                    );
                });
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let res = run(app2.clone(), &slots, stop_rx).await;
        for slot in &slots {
            if let Err(e) = &res {
                slot.rt.update(|s| {
                    s.state = "error".into();
                    s.error = Some(e.clone());
                });
            } else {
                slot.rt.update(|s| s.state = "offline".into());
            }
        }
        if let Err(e) = &res {
            tracing::error!(error = %e, "sol runtime exited with error");
        }
        rt.running.store(false, Ordering::SeqCst);
    });
}

/// Wait `secs`, returning `true` when stop was requested meanwhile.
async fn stopped_or_sleep(stop_rx: &mut watch::Receiver<bool>, secs: u64) -> bool {
    tokio::select! {
        _ = stop_rx.changed() => *stop_rx.borrow(),
        _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => false,
    }
}

/// Routing decision for one event. Pure over the slots' (wallet,
/// primary, paused) view so the exactly-one-engine + pause-isolation
/// invariants are unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dispatch {
    /// Execute on `slots[i]`.
    Run(usize),
    /// `slots[i]` owns the event but is paused — skip, loudly.
    Paused(usize),
    /// No local engine owns this event (stamped for a wallet that is
    /// not in this vault) — ignore.
    NoEngine,
}

/// `view[i] = (wallet, primary, paused)`. Stamped events go to exactly
/// the matching wallet; unstamped (legacy) events go to the primary
/// ONLY — see `degenbox_signer_core::wallet_event_is_mine`.
fn pick_engine(view: &[(String, bool, bool)], event_wallet: Option<&str>) -> Dispatch {
    for (i, (wallet, primary, paused)) in view.iter().enumerate() {
        if wallet_event_is_mine(event_wallet, wallet, *primary) {
            return if *paused {
                Dispatch::Paused(i)
            } else {
                Dispatch::Run(i)
            };
        }
    }
    Dispatch::NoEngine
}

fn slots_view(slots: &[EngineSlot]) -> Vec<(String, bool, bool)> {
    slots
        .iter()
        .map(|s| {
            (
                s.wallet.clone(),
                s.primary,
                s.pause_gate.lock().map(|g| *g).unwrap_or(false),
            )
        })
        .collect()
}

/// Apply a status update to every slot's telemetry.
fn update_all(slots: &[EngineSlot], f: impl Fn(&mut SolRuntimeStatus)) {
    for s in slots {
        s.rt.update(&f);
    }
}

async fn run(
    app: tauri::AppHandle,
    slots: &[EngineSlot],
    mut stop_rx: watch::Receiver<bool>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    let cfg = SolConfig::load_or_default();
    update_all(slots, |s| {
        *s = SolRuntimeStatus {
            state: "waiting_auth".into(),
            // Slice 8: copy buys are always armed; per-config budgets
            // are enforced server-side + clamped via max_spend.
            copy_armed: true,
            copy_session_sol: None,
            ..Default::default()
        };
    });

    // 1. Gateway credentials + our user uuid, resolved together and
    //    RE-RESOLVED per attempt. Retry forever (15 s) — the web app
    //    may push a session token to the :5829 daemon at any time, the
    //    user may pair or re-login at any time. Status surfaces the
    //    wait so it is never a silent dead state, and a 401/403 from
    //    `/auth/me` (expired/revoked token) flips to `auth_expired`
    //    instead of looping behind a generic error. Mirrors
    //    `watch_sells_cmd` step 1.
    let (auth, user_id): (GatewayAuth, _) = loop {
        if *stop_rx.borrow() {
            return Ok(());
        }
        let resolved = match gateway::resolve_auth(&state).await {
            Ok(a) => a,
            Err(e) => {
                update_all(slots, |s| {
                    s.state = "waiting_auth".into();
                    s.error = Some(e.clone());
                });
                if stopped_or_sleep(&mut stop_rx, 15).await {
                    return Ok(());
                }
                continue;
            }
        };
        // Don't clobber a sticky `auth_expired` with "connecting" on
        // every 15 s retry — the user would see the status flap while
        // the token is still dead. A successful probe (or fresh ready)
        // clears it below.
        update_all(slots, |s| {
            if s.state != "auth_expired" {
                s.state = "connecting".into();
                s.error = None;
            }
        });
        let relay_probe = RelayClient::new(resolved.base.clone(), resolved.token.clone());
        match relay_probe.fetch_user_id().await {
            Ok(id) => break (resolved, id),
            Err(RelayError::Status(code, body)) if code == 401 || code == 403 => {
                tracing::warn!(
                    code,
                    "auth/me rejected the resolved token — re-login required"
                );
                // `auth_expired` is sticky AND drives the shell's
                // access-loss lock. An expired signature is a normal
                // token lifecycle (re-login), not a revocation — keep
                // it in `waiting_auth` so the app stays usable while
                // the user re-links Discord.
                if code == 401 && body.contains("ExpiredSignature") {
                    update_all(slots, |s| {
                        s.state = "waiting_auth".into();
                        s.error = Some(
                            "gateway session expired — re-link your Discord account \
                             (account menu, top right) to sign back in"
                                .into(),
                        );
                    });
                } else {
                    update_all(slots, |s| {
                        s.state = "auth_expired".into();
                        s.error = Some(format!(
                            "gateway session expired (auth/me {code}: {body}) — \
                             re-login from the account menu (top right)"
                        ));
                    });
                }
            }
            Err(e) => {
                update_all(slots, |s| {
                    s.error = Some(format!("auth/me failed: {e}"));
                });
            }
        }
        if stopped_or_sleep(&mut stop_rx, 15).await {
            return Ok(());
        }
    };
    update_all(slots, |s| {
        // The probe succeeded, so any sticky auth_expired is over.
        s.state = "connecting".into();
        s.user_id = Some(user_id.to_string());
        s.error = None;
    });

    // 2. Per-attempt credential plumbing for the push streams (audit
    //    H1): every websocket (re)connect re-runs the FULL resolve
    //    chain — desktop-login JWT (kept fresh by the maintenance tick
    //    below) → HL pairing JWT → web-pushed session token — so a
    //    refreshed or re-issued token is picked up without restarting
    //    the runtime.
    let provider: TokenProvider = {
        let app = app.clone();
        Arc::new(move || {
            let app = app.clone();
            Box::pin(async move {
                let state = app.state::<AppState>();
                let a = gateway::resolve_auth(&state).await?;
                Ok(StreamAuth {
                    gateway_base: a.base,
                    token: a.token,
                })
            })
        })
    };
    //    Stream health → truthful status: an auth-rejected upgrade
    //    flips every slot to `auth_expired` (live HTTP 401/403) or
    //    `waiting_auth` (no usable credential — re-login, audit N3);
    //    the first successful (re)subscribe flips back to `ready`.
    //    Transient disconnects stay log-only (the subscribers own
    //    their reconnect loops).
    let health: StreamHealthSink = {
        let rts: Vec<SharedSolRuntime> = slots.iter().map(|s| s.rt.clone()).collect();
        Arc::new(move |h: StreamHealth| match h {
            StreamHealth::AuthFailed { message } => {
                let next = stream_auth_failed_state(&message);
                for rt in &rts {
                    rt.update(|s| {
                        s.state = next.into();
                        s.error = Some(format!(
                            "gateway session expired — re-login from the account menu (top right) ({message})"
                        ));
                    });
                }
            }
            StreamHealth::Subscribed => {
                for rt in &rts {
                    rt.update(|s| {
                        if s.state == "auth_expired" || s.state == "waiting_auth" {
                            s.state = "ready".into();
                            s.error = None;
                        }
                    });
                }
            }
            StreamHealth::Disconnected { .. } => {}
        })
    };

    // 3. Subscribe to the user-scoped streams ONCE for the whole
    //    fleet. The subscribers own their reconnect loops; dropping the
    //    receivers stops them. The single consumption point is the
    //    double-execution guarantee — events are fanned out by wallet
    //    below, never re-consumed.
    let mut sell_rx = spawn_sell_subscriber_with(provider.clone(), user_id, Some(health.clone()))
        .await
        .map_err(|e| format!("sell stream subscribe: {e}"))?;
    // Manual dashboard buys/sells: the web app pre-creates a
    // `trading_intents` row (`POST /api/trading/intents`) then relies on
    // a gateway-connected signer to fill it. We submit to that EXISTING
    // row (never a second) — the copy/sell streams instead create their
    // own. Routed to the PRIMARY engine (manual intents carry no wallet
    // stamp; see `handle_manual`).
    let mut intent_rx =
        spawn_intent_subscriber_with(provider.clone(), user_id, Some(health.clone()))
            .await
            .map_err(|e| format!("intent stream subscribe: {e}"))?;

    // 4. One engine PER WALLET — sells bypass matcher/budget by
    //    construction (`execute_sell`). Sizing/budget policy is
    //    server-side per copy config (slice 6/8); each event's
    //    `max_spend_lamports` is the clamp the signer honors.
    //
    //    RPC: user override → SOLANA_RPC_URL → the gateway's
    //    token-gated RPC proxy (zero-config default, slice 8). When the
    //    proxy default is active the URL embeds the auth token, so a
    //    credential rotation must rotate the RPC handles too (below).
    let uses_proxy_default = cfg.rpc_override().is_none();
    let rpc_url = cfg.resolved_rpc_url_with_auth(Some((&auth.base, &auth.token)));
    let mut engines: Vec<BotEngine> = Vec::with_capacity(slots.len());
    for _ in slots {
        let matcher = PresetMatcher {
            min_mcap_usd: None,
            max_mcap_usd: None,
            min_liquidity_usd: None,
            max_age_secs: None,
            blocked_tokens: Default::default(),
        };
        // Slice 8: the per-unlock client budget is retired — the
        // BudgetState stays as a pure spend LEDGER (unlimited caps)
        // feeding the Status page's spent counter.
        let budget = BudgetState::new(BudgetConfig {
            session_budget_lamports: u64::MAX,
            per_token_cap_lamports: None,
            per_hour_cap_lamports: None,
        });
        let allowlist = default_allowlist().map_err(|e| format!("allowlist: {e}"))?;
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
        engines.push(BotEngine::new(matcher, budget, allowlist, bot_cfg));
    }
    let jup = JupiterClient::new();
    let mut relay = RelayClient::new(auth.base.clone(), auth.token.clone());
    let mut relay_auth = (auth.base.clone(), auth.token.clone());
    let mut rpc = RpcClient::new(rpc_url);

    update_all(slots, |s| {
        s.state = "ready".into();
        s.alive_at = Some(Utc::now().to_rfc3339());
    });
    tracing::info!(
        %user_id,
        uses_proxy_default,
        engines = slots.len(),
        wallets = ?slots.iter().map(|s| short(&s.wallet)).collect::<Vec<_>>(),
        "sol runtime ready (sell stream live, per-wallet engines)"
    );

    // Maintenance tick: proactively renew the desktop-login JWT well
    // before its ~24 h expiry (audit H1). First tick fires immediately,
    // then every 30 min — `refresh_desktop_auth_if_needed` no-ops while
    // > 12 h of lifetime remain, so this is two cheap disk reads most
    // of the time.
    let mut refresh_tick = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
    refresh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Liveness heartbeat: stamp `alive_at` every 30 s while the loop is
    // alive so the UI can show a truthful engine heartbeat even when no
    // events arrive for hours.
    let mut hb_tick = tokio::time::interval(std::time::Duration::from_secs(30));
    hb_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // 5. Consume events until stop. Per-event failures log + continue —
    //    a stale TP must never nuke the loop (CLI parity).
    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    tracing::info!("sol runtime stopping (lock/quit)");
                    return Ok(());
                }
            }
            _ = hb_tick.tick() => {
                let now = Utc::now().to_rfc3339();
                update_all(slots, |s| s.alive_at = Some(now.clone()));
            }
            _ = refresh_tick.tick() => {
                match gateway::refresh_desktop_auth_if_needed().await {
                    Ok(Some(token)) => {
                        // Feed the :5829 daemon + rotate the REST client
                        // so web-app probes and intent submits ride the
                        // fresh token immediately.
                        crate::auth::install_runtime_token(&app, &token).await;
                        if refresh_relay(&state, &mut relay, &mut relay_auth).await
                            && uses_proxy_default
                        {
                            rotate_proxy_rpc(&mut rpc, &mut engines, &relay_auth);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e,
                            "desktop JWT refresh failed — retrying next tick");
                    }
                }
            }
            evt = sell_rx.recv() => {
                let Some(evt) = evt else {
                    return Err("sell stream ended — relock/unlock to restart".into());
                };
                if refresh_relay(&state, &mut relay, &mut relay_auth).await && uses_proxy_default {
                    rotate_proxy_rpc(&mut rpc, &mut engines, &relay_auth);
                }
                let kind = match evt.trigger_kind {
                    TriggerKind::Tp => "TP sell",
                    TriggerKind::Sl => "SL sell",
                };
                match pick_engine(&slots_view(slots), evt.wallet_pubkey.as_deref()) {
                    Dispatch::Run(i) => {
                        let slot = &slots[i];
                        handle_sell(&state, &slot.rt, &mut engines[i], &jup, &relay, &rpc, slot.kp.as_ref(), evt).await;
                    }
                    Dispatch::Paused(i) => {
                        let slot = &slots[i];
                        tracing::info!(mint = %short(&evt.mint), wallet = %short(&slot.wallet),
                            "sell trigger skipped — client paused");
                        push(&state, kind, &evt.mint, "skipped (paused)");
                        mark_event(&slot.rt, false, true);
                    }
                    Dispatch::NoEngine => {
                        tracing::warn!(mint = %short(&evt.mint), wallet = ?evt.wallet_pubkey,
                            "sell trigger stamped for a wallet not in this vault — ignored");
                    }
                }
            }
            evt = intent_rx.recv() => {
                let Some(evt) = evt else {
                    return Err("intent stream ended — relock/unlock to restart".into());
                };
                if refresh_relay(&state, &mut relay, &mut relay_auth).await && uses_proxy_default {
                    rotate_proxy_rpc(&mut rpc, &mut engines, &relay_auth);
                }
                // Only USER-created manual intents. Copy / TP-SL / bot
                // intents are announced on this same subject at create
                // time but are submitted by their own path — the
                // `is_untagged` fast-path + the authoritative status
                // re-check in `handle_manual` keep us from double-filling
                // them.
                if !evt.is_untagged() {
                    tracing::debug!(intent = %short(&evt.id.to_string()),
                        "intent skipped — copy/bot-tagged (not manual)");
                } else if evt.side != "buy" && evt.side != "sell" {
                    tracing::warn!(side = %evt.side, "manual intent with unknown side — ignored");
                } else {
                    // Manual intents carry no wallet stamp; route to the
                    // PRIMARY engine exactly like a legacy unstamped event
                    // (exactly-one-engine rule preserved).
                    match pick_engine(&slots_view(slots), None) {
                        Dispatch::Run(i) => {
                            let slot = &slots[i];
                            handle_manual(
                                &state,
                                &slot.rt,
                                &mut engines[i],
                                &jup,
                                &relay,
                                &rpc,
                                slot.kp.as_ref(),
                                evt,
                            )
                            .await;
                        }
                        Dispatch::Paused(i) => {
                            let slot = &slots[i];
                            let token = if evt.side == "sell" { &evt.input_mint } else { &evt.output_mint };
                            tracing::info!(intent = %short(&evt.id.to_string()), wallet = %short(&slot.wallet),
                                "manual intent skipped — client paused");
                            push(&state, if evt.side == "sell" { "manual sell" } else { "manual buy" }, token, "skipped (paused)");
                            mark_event(&slot.rt, false, evt.side == "sell");
                        }
                        Dispatch::NoEngine => {
                            tracing::warn!(intent = %short(&evt.id.to_string()),
                                "manual intent but no primary Sol engine in this vault — ignored");
                        }
                    }
                }
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

/// Rebuild the relay REST client when the resolve chain yields a
/// different credential than the one it was built with — keeps intent
/// creates / submits riding the freshest token after a refresh or
/// re-login, without touching `RelayClient` itself. Resolve failures
/// keep the old client (its next call 401s and the handler flags it).
/// Returns `true` when the credentials rotated, so a caller running on
/// the token-gated gateway RPC proxy can rotate its RPC handles too.
async fn refresh_relay(
    state: &AppState,
    relay: &mut RelayClient,
    current: &mut (String, String),
) -> bool {
    if let Ok(a) = gateway::resolve_auth(state).await {
        if a.base != current.0 || a.token != current.1 {
            tracing::info!("gateway credentials rotated — rebuilding relay REST client");
            *relay = RelayClient::new(a.base.clone(), a.token.clone());
            *current = (a.base, a.token);
            return true;
        }
    }
    false
}

/// Rotate the RPC client + every engine's simulator URL onto the
/// freshly-rotated credential. Only meaningful while the gateway RPC
/// proxy default is active (the URL embeds the token); user-override /
/// env RPC setups never call this.
fn rotate_proxy_rpc(rpc: &mut RpcClient, engines: &mut [BotEngine], auth: &(String, String)) {
    let url = degenbox_signer_core::gateway_proxy_rpc_url(&auth.0, &auth.1);
    tracing::info!("gateway credentials rotated — rebuilding proxy RPC handles");
    *rpc = RpcClient::new(url.clone());
    for e in engines.iter_mut() {
        e.set_rpc_url(url.clone());
    }
}

/// Does this per-event error string look like a relay/gateway auth
/// rejection? `RelayError::Status` renders as "gateway responded with
/// status 401: …" (also wrapped as "relay: …" by `BotError`), and the
/// read-side gateway client renders "gateway 401 Unauthorized: …".
fn is_auth_error(err: &str) -> bool {
    err.contains("status 401")
        || err.contains("status 403")
        || err.contains("gateway 401")
        || err.contains("gateway 403")
}

/// An expired JWT signature is the credential's NORMAL lifecycle, never
/// a revocation. The gateway surfaces jsonwebtoken's error verbatim in
/// 401 bodies (`ExpiredSignature` — `crates/platform/auth`'s
/// transparent `Jwt` Display), and both `RelayError::Status` and the
/// read-side gateway client carry the body through into the error
/// string this matches on.
fn is_expired_token_error(err: &str) -> bool {
    err.contains("ExpiredSignature")
}

/// Classify a stream-level `AuthFailed` into the runtime state it
/// should drive (audit 2026-06-12 N3). Two message shapes arrive from
/// `sell_stream`/`copy_stream`:
///
/// 1. `"gateway rejected the stream token (HTTP 401)"` — a live
///    upgrade rejection. The WS handshake exposes no response body, so
///    expiry vs revocation is undecidable HERE → `auth_expired`, and
///    the shell confirms with an `access_check` second opinion before
///    treating it as access loss (App.tsx fast path — `access_check`
///    routes `ExpiredSignature` to re-login, never lock).
/// 2. The `TokenProvider`'s own resolve failure ("gateway session
///    expired — re-link…" / "not connected to DegenBox — …"): nothing
///    was rejected, we simply hold no usable credential right now.
///    That is `waiting_auth` (re-login), exactly like the bootstrap
///    loop's resolve failure — it must never escalate to the sticky
///    access-loss state.
fn stream_auth_failed_state(message: &str) -> &'static str {
    if message.contains("HTTP 401") || message.contains("HTTP 403") {
        "auth_expired"
    } else {
        "waiting_auth"
    }
}

/// Mirror of the stream-level 401-awareness for relay REST failures:
/// flip the slot to `auth_expired` so a dead session can't hide behind
/// per-event "failed" counters — UNLESS the 401 body says the token
/// merely expired (audit N3): that is the normal credential lifecycle,
/// surfaced as `waiting_auth` (re-login) so the shell's access-loss
/// lock (which also drops the cached keychain passphrase) never fires
/// for it. Mirrors the bootstrap probe + `access_check` (5ba2f30).
fn flag_auth_expired_if_401(rt: &SharedSolRuntime, err: &str) {
    if !is_auth_error(err) {
        return;
    }
    if is_expired_token_error(err) {
        rt.update(|s| {
            s.state = "waiting_auth".into();
            s.error = Some(
                "gateway session expired — re-link your Discord account \
                 (account menu, top right) to sign back in"
                    .into(),
            );
        });
        return;
    }
    rt.update(|s| {
        s.state = "auth_expired".into();
        s.error = Some(
            "gateway session expired — re-login from the account menu (top right) \
             (a relay call was rejected with 401/403)"
                .into(),
        );
    });
}

/// A successful relay round-trip proves the credentials work again —
/// clear an `auth_expired` / expiry-`waiting_auth` flag set by an
/// earlier REST 401 (the WS may never re-subscribe if it stayed
/// connected across the expiry, so the stream-level recovery path
/// can't be the only one).
fn restore_ready_after_success(rt: &SharedSolRuntime) {
    rt.update(|s| {
        if s.state == "auth_expired" || s.state == "waiting_auth" {
            s.state = "ready".into();
            s.error = None;
        }
    });
}

fn push(state: &AppState, kind: &str, identifier: &str, status: &str) {
    state.push_recent(RecentSign {
        at: Utc::now(),
        chain: "sol",
        kind: kind.to_string(),
        identifier: identifier.to_string(),
        status: status.to_string(),
    });
}

fn mark_event(rt: &SharedSolRuntime, ok: bool, sell: bool) {
    rt.update(|s| {
        let now = Utc::now().to_rfc3339();
        s.last_event_at = Some(now.clone());
        s.alive_at = Some(now);
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

/// TP/SL sell trigger — port of `watch_sells_cmd`'s event body.
#[allow(clippy::too_many_arguments)]
async fn handle_sell(
    state: &AppState,
    rt: &SharedSolRuntime,
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
            push(state, kind_label, &evt.mint, "skipped");
            mark_event(rt, false, true);
            return;
        }
    };
    tracing::info!(mint = %short(&evt.mint), amount = token_amount, kind = kind_label,
        price = %evt.triggered_at_price_usd, "sell trigger received");
    let dedupe_id = evt.leg_id.unwrap_or(evt.target_id).to_string();
    match engine
        .execute_sell_with_dedupe(
            Some(dedupe_id),
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
            push(state, kind_label, &evt.mint, "submitted");
            mark_event(rt, true, true);
            restore_ready_after_success(rt);
        }
        Ok(Decision::Skipped(r)) => {
            tracing::info!(mint = %short(&evt.mint), reason = %r, "sell skipped");
            push(state, kind_label, &evt.mint, "skipped");
            mark_event(rt, false, true);
        }
        Err(e) => {
            tracing::warn!(mint = %short(&evt.mint), error = %e, "sell failed");
            push(state, kind_label, &evt.mint, "failed");
            rt.update(|s| s.error = Some(format!("{kind_label} {}: {e}", short(&evt.mint))));
            mark_event(rt, false, true);
            flag_auth_expired_if_401(rt, &e.to_string());
        }
    }
}

/// WSOL mint — the fixed SOL leg of every dashboard buy/sell.
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Manual (dashboard) buy/sell execution.
///
/// Unlike copy/TP-SL, the `trading_intents` row ALREADY EXISTS (the web
/// UI created it so it could render a `pending` state). We fill THAT row:
/// [`BotEngine::set_pending_intent_id`] makes the shared quote→build→
/// sign→relay path submit the existing intent instead of creating a
/// second one (mirrors the daemon `/swap` `intentId` path). Safety gates,
/// in order:
///
/// 1. **Expiry** — refuse an intent past `expires_at` (the gateway's
///    `claim_intent_for_submit` gates on it too).
/// 2. **Authoritative status re-check** (`GET /api/trading/intents/{id}`)
///    — the WS payload's `pending` is a create-time snapshot; every
///    signer-created intent is announced here and the local daemon / a
///    second device may already have filled a real manual one. We fill
///    ONLY a row the gateway still reports `pending`, untagged, and not
///    client-scoped. Fail-closed on a read error.
/// 3. **Atomic claim** (server-side) — the final double-submit authority;
///    a lost race returns a benign 409 we treat as "already handled".
///
/// The stamp is CLEARED after every call so a skipped/failed manual can
/// never leak its id into the next copy/TP-SL/bot trade on this engine.
#[allow(clippy::too_many_arguments)]
async fn handle_manual(
    state: &AppState,
    rt: &SharedSolRuntime,
    engine: &mut BotEngine,
    jup: &JupiterClient,
    relay: &RelayClient,
    rpc: &RpcClient,
    kp: &Keypair,
    evt: ManualIntentEvent,
) {
    let is_sell = evt.side == "sell";
    let label = if is_sell { "manual sell" } else { "manual buy" };
    // The token side (for buys the output, for sells the input); the
    // other leg is always WSOL on the dashboard path.
    let token_mint = if is_sell {
        evt.input_mint.clone()
    } else {
        evt.output_mint.clone()
    };
    let intent_id = evt.id.to_string();

    // Guard the SOL leg: the engine always swaps against WSOL, so an
    // intent whose SOL leg is a different mint would fill with the wrong
    // route. The dashboard never produces this — refuse rather than guess.
    let sol_leg_ok = if is_sell {
        evt.output_mint == WSOL_MINT
    } else {
        evt.input_mint == WSOL_MINT
    };
    if !sol_leg_ok {
        tracing::warn!(intent = %short(&intent_id),
            "manual intent SOL leg is not WSOL — unsupported, skipped");
        push(state, label, &token_mint, "skipped (unsupported)");
        return;
    }

    // 1. Expiry.
    if evt.expires_at <= Utc::now() {
        tracing::info!(intent = %short(&intent_id), "manual intent expired — skipped");
        push(state, label, &token_mint, "skipped (expired)");
        return;
    }

    // 2. Authoritative status re-check — fail-closed.
    match relay.get_intent(&intent_id).await {
        Ok(row) => {
            if row.status != "pending" {
                tracing::info!(intent = %short(&intent_id), status = %row.status,
                    "manual intent no longer pending — skipped");
                return;
            }
            if row.copy_config_id.is_some() || row.bot_session_id.is_some() {
                tracing::debug!(intent = %short(&intent_id),
                    "intent is copy/bot-tagged — not manual, skipped");
                return;
            }
            if row.client_id.is_some() {
                // The wire carries no wallet pubkey and the gateway
                // `trading_clients.id` is not the local vault id, so we
                // can't map it to a wallet. Refuse rather than spend on
                // the wrong wallet (deferred: multi-client manual routing).
                tracing::warn!(intent = %short(&intent_id), client_id = ?row.client_id,
                    "manual intent scoped to a specific client — can't resolve to a local wallet, skipped");
                push(state, label, &token_mint, "skipped (client-scoped)");
                return;
            }
        }
        Err(RelayError::Status(404, _)) => {
            tracing::info!(intent = %short(&intent_id), "manual intent gone (404) — skipped");
            return;
        }
        Err(e) => {
            tracing::warn!(intent = %short(&intent_id), error = %e,
                "manual intent status recheck failed — skipped (fail-closed)");
            flag_auth_expired_if_401(rt, &e.to_string());
            return;
        }
    }

    // 3. Execute against the EXISTING intent, then always clear the stamp.
    let slippage = u16::try_from(evt.slippage_bps.clamp(1, 10_000)).unwrap_or(100);
    tracing::info!(intent = %short(&intent_id), mint = %short(&token_mint), side = %evt.side,
        amount = evt.amount_in_lamports, "manual intent executing");
    engine.set_pending_intent_id(Some(intent_id.clone()));
    let result = if is_sell {
        let token_amount = evt.amount_in_lamports.max(0) as u64;
        engine
            .execute_sell_with_dedupe(
                Some(intent_id.clone()),
                token_mint.clone(),
                token_amount,
                None,
                jup,
                relay,
                rpc,
                kp,
            )
            .await
    } else {
        let lamports = evt.amount_in_lamports.max(0) as u64;
        engine
            .execute_buy(
                intent_id.clone(),
                token_mint.clone(),
                lamports,
                Some(slippage),
                None,
                jup,
                relay,
                rpc,
                kp,
            )
            .await
    };
    engine.set_pending_intent_id(None);

    match result {
        Ok(Decision::Submitted(_)) => {
            push(state, label, &token_mint, "submitted");
            mark_event(rt, true, is_sell);
            restore_ready_after_success(rt);
        }
        Ok(Decision::Skipped(r)) => {
            tracing::info!(intent = %short(&intent_id), reason = %r, "manual intent skipped");
            push(state, label, &token_mint, "skipped");
            mark_event(rt, false, is_sell);
        }
        Err(e) => {
            let es = e.to_string();
            // Lost the server-side claim race (local daemon / another
            // device won) → 409. Benign, not a failure.
            if es.contains("status 409") {
                tracing::info!(intent = %short(&intent_id), detail = %es,
                    "manual intent already handled elsewhere — skipped");
                push(state, label, &token_mint, "skipped (already handled)");
                mark_event(rt, false, is_sell);
            } else {
                tracing::warn!(intent = %short(&intent_id), error = %es, "manual intent failed");
                push(state, label, &token_mint, "failed");
                rt.update(|s| s.error = Some(format!("{label} {}: {e}", short(&token_mint))));
                mark_event(rt, false, is_sell);
                flag_auth_expired_if_401(rt, &es);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Vec<(String, bool, bool)> {
        vec![
            ("Wprimary".to_string(), true, false),
            ("Wsecond".to_string(), false, false),
            ("Wthird".to_string(), false, false),
        ]
    }

    /// Safety test (b): a legacy event (no executor stamp) dispatches to
    /// exactly ONE engine — the primary — never to a secondary.
    #[test]
    fn legacy_event_dispatches_to_primary_engine_only() {
        assert_eq!(pick_engine(&view(), None), Dispatch::Run(0));
        // Exactly one: re-run against every rotation of the primary flag.
        for primary_idx in 0..3 {
            let v: Vec<(String, bool, bool)> = view()
                .into_iter()
                .enumerate()
                .map(|(i, (w, _, p))| (w, i == primary_idx, p))
                .collect();
            assert_eq!(pick_engine(&v, None), Dispatch::Run(primary_idx));
        }
    }

    #[test]
    fn stamped_event_dispatches_to_exactly_the_matching_engine() {
        assert_eq!(pick_engine(&view(), Some("Wsecond")), Dispatch::Run(1));
        assert_eq!(pick_engine(&view(), Some("Wprimary")), Dispatch::Run(0));
        // Stamped for a wallet this vault doesn't hold → nothing runs.
        assert_eq!(pick_engine(&view(), Some("Wforeign")), Dispatch::NoEngine);
    }

    /// Safety test (c): pausing one client skips ITS events only —
    /// sibling engines keep executing theirs.
    #[test]
    fn paused_client_skips_its_events_without_touching_siblings() {
        let mut v = view();
        v[1].2 = true; // pause Wsecond
        assert_eq!(pick_engine(&v, Some("Wsecond")), Dispatch::Paused(1));
        // Siblings unaffected:
        assert_eq!(pick_engine(&v, Some("Wthird")), Dispatch::Run(2));
        assert_eq!(pick_engine(&v, None), Dispatch::Run(0));
        // A paused PRIMARY skips legacy events too, but a stamped
        // sibling event still executes.
        let mut v = view();
        v[0].2 = true;
        assert_eq!(pick_engine(&v, None), Dispatch::Paused(0));
        assert_eq!(pick_engine(&v, Some("Wsecond")), Dispatch::Run(1));
    }

    /// Relay/gateway auth rejections must be recognised in the exact
    /// formats `RelayError::Status` / `BotError::Relay` / the read-side
    /// gateway client produce — and ONLY those.
    #[test]
    fn auth_error_classifier_matches_relay_and_gateway_formats() {
        // RelayError::Status via BotError::Relay.
        assert!(is_auth_error(
            "relay: gateway responded with status 401: token expired"
        ));
        assert!(is_auth_error("gateway responded with status 403: nope"));
        // gateway.rs get_json shape ("gateway {status}: {body}").
        assert!(is_auth_error("GET /x: gateway 401 Unauthorized: "));
        // Non-auth failures stay non-auth.
        assert!(!is_auth_error(
            "relay: gateway responded with status 500: boom"
        ));
        assert!(!is_auth_error("jupiter: quote timeout"));
        assert!(!is_auth_error("rpc: connection refused"));
    }

    /// `auth_expired` flips on a 401-class error and back to `ready`
    /// after the next successful relay round-trip.
    #[test]
    fn auth_expired_flag_sets_and_clears() {
        let rt: SharedSolRuntime = Arc::new(SolRuntimeInner::default());
        rt.update(|s| s.state = "ready".into());

        flag_auth_expired_if_401(&rt, "relay: gateway responded with status 401: expired");
        assert_eq!(rt.snapshot().state, "auth_expired");
        assert!(rt.snapshot().error.is_some());

        restore_ready_after_success(&rt);
        assert_eq!(rt.snapshot().state, "ready");
        assert!(rt.snapshot().error.is_none());

        // Non-auth errors must NOT flip the state…
        flag_auth_expired_if_401(&rt, "relay: gateway responded with status 500: boom");
        assert_eq!(rt.snapshot().state, "ready");
        // …and restore never touches unrelated states.
        rt.update(|s| s.state = "error".into());
        restore_ready_after_success(&rt);
        assert_eq!(rt.snapshot().state, "error");
        rt.update(|s| s.state = "offline".into());
        restore_ready_after_success(&rt);
        assert_eq!(rt.snapshot().state, "offline");
    }

    /// Audit 2026-06-12 N3: a 401 whose body says the token merely
    /// EXPIRED is the credential's normal lifecycle — it must surface
    /// as `waiting_auth` (re-login), NEVER as the sticky `auth_expired`
    /// that drives the shell's vault lock + keychain-passphrase drop.
    #[test]
    fn expired_signature_401_is_relogin_not_access_loss() {
        let rt: SharedSolRuntime = Arc::new(SolRuntimeInner::default());
        rt.update(|s| s.state = "ready".into());

        flag_auth_expired_if_401(
            &rt,
            "relay: gateway responded with status 401: invalid token: ExpiredSignature",
        );
        let snap = rt.snapshot();
        assert_eq!(snap.state, "waiting_auth");
        assert!(snap.error.as_deref().unwrap_or("").contains("re-link"));

        // A later successful relay round-trip (re-login picked up by
        // refresh_relay) restores `ready`.
        restore_ready_after_success(&rt);
        assert_eq!(rt.snapshot().state, "ready");
        assert!(rt.snapshot().error.is_none());

        // Read-side gateway client shape carries the body too.
        flag_auth_expired_if_401(&rt, "GET /x: gateway 401 Unauthorized: ExpiredSignature");
        assert_eq!(rt.snapshot().state, "waiting_auth");
    }

    /// Stream `AuthFailed` classification (audit N3): a live HTTP
    /// 401/403 upgrade rejection is `auth_expired` (the shell then
    /// gets an `access_check` second opinion before any lock); the
    /// TokenProvider's own "no usable credential" resolve failure is
    /// `waiting_auth` — nothing was rejected, so it must never reach
    /// the access-loss state at all.
    #[test]
    fn stream_auth_failure_classifies_reject_vs_no_credential() {
        // sell_stream/copy_stream upgrade-rejection shape.
        assert_eq!(
            stream_auth_failed_state("gateway rejected the stream token (HTTP 401)"),
            "auth_expired"
        );
        assert_eq!(
            stream_auth_failed_state("gateway rejected the stream token (HTTP 403)"),
            "auth_expired"
        );
        // resolve_auth failure shapes (gateway.rs) — no credential to
        // reject: re-login, not access loss.
        assert_eq!(
            stream_auth_failed_state(
                "gateway session expired — re-link your Discord account (account menu, top right) to \
                 sign back in"
            ),
            "waiting_auth"
        );
        assert_eq!(
            stream_auth_failed_state(
                "not connected to DegenBox — link your Discord account (account menu, top right), \
                 pair this signer, or open the DegenBox web app once"
            ),
            "waiting_auth"
        );
    }
}
