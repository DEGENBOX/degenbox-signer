//! Trade-settings + position-management IPC surface (Wave-5 "everything
//! in the bot too").
//!
//! Thin authenticated passthroughs to gateway endpoints the web app
//! already exercises (copy-config CRUD, TP/SL ladders, bot sessions,
//! HL close/tpsl) plus the local-execution bridges that only this app
//! can offer:
//!
//! - **Sells** execute through the same engine the bots use — native
//!   PumpFun/PumpSwap/Raydium routing with Jupiter fallback — clamped
//!   to the wallet's ON-CHAIN balance, never a stale gateway row, and
//!   ROUTED to the wallet that actually holds the position (audit N2):
//!   the primary signs via the app's own `:5829` daemon
//!   (`/quote` → `/swap`), secondaries via a direct `BotEngine` run
//!   with their own vault keypair. Ambiguous holders refuse loudly.
//! - **Bot arming** creates the gateway session row, then arms the
//!   in-process daemon via `/bot/enable` — the same contract the web
//!   app drives, so "armed on this device" stays truthful on both
//!   surfaces.
//!
//! Every command resolves gateway auth per-call (`gateway::resolve_auth`)
//! and returns errors verbatim for the GUI's standard error box.

use crate::sol::config::SolConfig;
use crate::sol::gateway::{self, GatewayAuth};
use crate::state::AppState;
use degenbox_signer_core as core;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tauri::State;

const WSOL: &str = "So11111111111111111111111111111111111111112";

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("reqwest client")
}

/// Gateway request helper — bearer-authed, body optional, returns the
/// raw response text (empty on 204). Errors carry the gateway's own
/// message so validation failures surface verbatim in the UI.
async fn gw(
    auth: &GatewayAuth,
    method: reqwest::Method,
    path: &str,
    body: Option<&serde_json::Value>,
) -> Result<String, String> {
    let url = format!("{}{}", auth.base, path);
    let mut req = http()
        .request(method.clone(), &url)
        .bearer_auth(&auth.token);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("{method} {path}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Machine-readable gateway errors ({"error": …, "message": …})
        // pass through — the UI renders the reason.
        return Err(format!("gateway {status}: {text}"));
    }
    Ok(text)
}

fn parse<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|e| format!("{what} decode: {e}"))
}

// ─── local `:5829` daemon bridge ────────────────────────────────────

fn daemon_base(state: &AppState) -> Result<String, String> {
    let g = state
        .local_daemon
        .lock()
        .map_err(|_| "daemon status poisoned".to_string())?;
    if !g.running {
        return Err(g
            .error
            .clone()
            .unwrap_or_else(|| "local signer daemon is not running".into()));
    }
    Ok(format!("http://127.0.0.1:{}", g.port))
}

/// Resolve gateway auth and push it into the in-process daemon's
/// runtime config (base + token) so `/swap` and `/bot/enable` are
/// authenticated even when no web app ever pushed a `/setAuth`.
async fn ensure_daemon_auth(state: &AppState) -> Result<GatewayAuth, String> {
    let auth = gateway::resolve_auth(state).await?;
    let rc = state.web_auth.lock().ok().and_then(|g| g.as_ref().cloned());
    if let Some(rc) = rc {
        let mut g = rc.write().await;
        g.auth_token = Some(auth.token.clone());
        g.gateway_base = auth.base.clone();
    }
    Ok(auth)
}

/// POST to the local daemon; surfaces its `{"error": …}` payloads as
/// readable messages ("signer locked — …", "no auth token — …").
async fn daemon_post(
    base: &str,
    path: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let resp = http()
        .post(format!("{base}{path}"))
        .json(body)
        .send()
        .await
        .map_err(|e| format!("daemon {path}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or(text);
        return Err(format!("signer daemon: {msg}"));
    }
    serde_json::from_str(&text).map_err(|e| format!("daemon {path} decode: {e}"))
}

// ─── Solana copy-config CRUD ────────────────────────────────────────

/// Full Sol copy-config row, leader-resolved — every editable field the
/// web's CopyTradePanel exposes. `min_source_buy_usd` stays a decimal
/// string; lamports/bps stay integers (the UI converts for display).
#[derive(Debug, Serialize)]
pub struct SolCopyConfigFull {
    pub id: String,
    pub tracked_wallet_id: String,
    pub leader: String,
    pub label: String,
    pub enabled: bool,
    pub sizing_mode: i16,
    pub fixed_sol_lamports: Option<i64>,
    pub pct_of_balance_bps: Option<i32>,
    pub max_position_sol_lamports: Option<i64>,
    /// Mode 2: % of the leader's buy size (int >= 1, 100 = mirror).
    pub buy_size_pct: Option<i32>,
    /// Mode 3: leader cash-fraction scale (int >= 1, 100 = mirror).
    pub balance_pct: Option<i32>,
    /// Per-config copy budget (lamports); None = uncapped.
    pub copy_budget_lamports: Option<i64>,
    /// Spend counts from this instant (manual reset bumps it).
    pub copy_budget_epoch: Option<String>,
    /// Opt-in: buy each token at most once per config.
    pub single_buy_per_token: bool,
    pub min_source_buy_usd: Option<String>,
    pub per_mint_cooldown_secs: i32,
    pub slippage_bps: i32,
    pub mirror_sells: bool,
    /// Raw `LegSpec[]` JSON (kind/trigger_pct/sell_fraction_bps).
    pub default_ladder: Option<serde_json::Value>,
    pub client_id: Option<String>,
    /// The tracked wallet's copy-feed switch — an enabled config with
    /// the feed off is silently dark; the UI warns + fixes on save.
    pub wallet_copy_mode: bool,
}

#[derive(Debug, Deserialize)]
struct GwCopyConfigFull {
    id: String,
    tracked_wallet_id: String,
    enabled: bool,
    sizing_mode: i16,
    #[serde(default)]
    fixed_sol_lamports: Option<i64>,
    #[serde(default)]
    pct_of_balance_bps: Option<i32>,
    #[serde(default)]
    max_position_sol_lamports: Option<i64>,
    #[serde(default)]
    buy_size_pct: Option<i32>,
    #[serde(default)]
    balance_pct: Option<i32>,
    #[serde(default)]
    copy_budget_lamports: Option<i64>,
    #[serde(default)]
    copy_budget_epoch: Option<String>,
    #[serde(default)]
    single_buy_per_token: bool,
    #[serde(default)]
    min_source_buy_usd: Option<String>,
    #[serde(default)]
    per_mint_cooldown_secs: i32,
    #[serde(default)]
    slippage_bps: i32,
    #[serde(default)]
    mirror_sells: bool,
    #[serde(default)]
    default_ladder: Option<serde_json::Value>,
    #[serde(default)]
    client_id: Option<String>,
}

/// One tracked wallet (leader candidate) for the create-config picker.
#[derive(Debug, Serialize, Deserialize)]
pub struct TrackedWalletDto {
    pub id: String,
    pub address: String,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub copy_mode: bool,
}

fn short(addr: &str) -> String {
    if addr.len() <= 10 {
        addr.to_string()
    } else {
        format!("{}…{}", &addr[..4], &addr[addr.len() - 4..])
    }
}

async fn fetch_tracked_wallets(auth: &GatewayAuth) -> Result<Vec<TrackedWalletDto>, String> {
    let text = gw(
        auth,
        reqwest::Method::GET,
        "/api/wallet-tracker/wallets",
        None,
    )
    .await?;
    parse(&text, "tracked wallets")
}

#[tauri::command]
pub async fn tracked_wallets_list(
    state: State<'_, AppState>,
) -> Result<Vec<TrackedWalletDto>, String> {
    let auth = gateway::resolve_auth(&state).await?;
    fetch_tracked_wallets(&auth).await
}

#[tauri::command]
pub async fn sol_copy_configs_full(
    state: State<'_, AppState>,
) -> Result<Vec<SolCopyConfigFull>, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::GET,
        "/api/trading/copy/configs",
        None,
    )
    .await?;
    let rows: Vec<GwCopyConfigFull> = parse(&text, "copy configs")?;
    let wallets = fetch_tracked_wallets(&auth).await.unwrap_or_default();
    Ok(rows
        .into_iter()
        .map(|r| {
            let w = wallets.iter().find(|w| w.id == r.tracked_wallet_id);
            let leader = w
                .map(|w| w.address.clone())
                .unwrap_or_else(|| r.tracked_wallet_id.clone());
            let label = w
                .and_then(|w| w.alias.clone())
                .filter(|a| !a.trim().is_empty())
                .unwrap_or_else(|| short(&leader));
            SolCopyConfigFull {
                id: r.id,
                tracked_wallet_id: r.tracked_wallet_id,
                leader,
                label,
                enabled: r.enabled,
                sizing_mode: r.sizing_mode,
                fixed_sol_lamports: r.fixed_sol_lamports,
                pct_of_balance_bps: r.pct_of_balance_bps,
                max_position_sol_lamports: r.max_position_sol_lamports,
                buy_size_pct: r.buy_size_pct,
                balance_pct: r.balance_pct,
                copy_budget_lamports: r.copy_budget_lamports,
                copy_budget_epoch: r.copy_budget_epoch,
                single_buy_per_token: r.single_buy_per_token,
                min_source_buy_usd: r.min_source_buy_usd,
                per_mint_cooldown_secs: r.per_mint_cooldown_secs,
                slippage_bps: r.slippage_bps,
                mirror_sells: r.mirror_sells,
                default_ladder: r.default_ladder,
                client_id: r.client_id,
                wallet_copy_mode: w.map(|w| w.copy_mode).unwrap_or(false),
            }
        })
        .collect())
}

/// `POST /api/trading/copy/configs` — body built (and unit-validated)
/// in the UI; the gateway re-validates everything server-side.
#[tauri::command]
pub async fn sol_copy_config_create(
    body: serde_json::Value,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::POST,
        "/api/trading/copy/configs",
        Some(&body),
    )
    .await?;
    parse(&text, "copy config")
}

/// `PATCH /api/trading/copy/configs/{id}` — partial update with the
/// gateway's clear-flag semantics (`clear_max_position`, …).
#[tauri::command]
pub async fn sol_copy_config_update(
    config_id: String,
    patch: serde_json::Value,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::PATCH,
        &format!("/api/trading/copy/configs/{config_id}"),
        Some(&patch),
    )
    .await?;
    parse(&text, "copy config")
}

#[tauri::command]
pub async fn sol_copy_config_delete(
    config_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = gateway::resolve_auth(&state).await?;
    gw(
        &auth,
        reqwest::Method::DELETE,
        &format!("/api/trading/copy/configs/{config_id}"),
        None,
    )
    .await
    .map(|_| ())
}

/// Lockstep helper: an enabled config needs the wallet's copy feed on
/// (`tracked_wallets.copy_mode`) or the engine never sees its trades.
#[tauri::command]
pub async fn tracked_wallet_set_copy_mode(
    wallet_id: String,
    copy_mode: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = gateway::resolve_auth(&state).await?;
    gw(
        &auth,
        reqwest::Method::PATCH,
        &format!("/api/wallet-tracker/wallets/{wallet_id}"),
        Some(&serde_json::json!({ "copy_mode": copy_mode })),
    )
    .await
    .map(|_| ())
}

// ─── TP/SL ladders (position targets) ───────────────────────────────

/// All target ladders for the user (live + historical heads, legs
/// attached). Raw gateway JSON — the TS layer types it.
#[tauri::command]
pub async fn sol_targets_list(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::GET,
        "/api/trading/positions/targets?limit=200",
        None,
    )
    .await?;
    parse(&text, "targets")
}

/// Live ladder on one position (`null` when nothing is armed).
#[tauri::command]
pub async fn sol_target_get(
    mint: String,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::GET,
        &format!("/api/trading/positions/{mint}/target"),
        None,
    )
    .await?;
    parse(&text, "target")
}

/// Arm or replace the live TP/SL ladder. Body =
/// `{ entry_price_usd, legs: [{kind, trigger_pct, sell_fraction_bps}] }`
/// — the exact web wire shape; the gateway validates + replaces
/// atomically.
#[tauri::command]
pub async fn sol_target_arm(
    mint: String,
    body: serde_json::Value,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::PUT,
        &format!("/api/trading/positions/{mint}/target"),
        Some(&body),
    )
    .await?;
    parse(&text, "target")
}

/// Disarm (cancel) the live ladder — idempotent on the gateway.
#[tauri::command]
pub async fn sol_target_disarm(mint: String, state: State<'_, AppState>) -> Result<(), String> {
    let auth = gateway::resolve_auth(&state).await?;
    gw(
        &auth,
        reqwest::Method::DELETE,
        &format!("/api/trading/positions/{mint}/target"),
        None,
    )
    .await
    .map(|_| ())
}

// ─── Sol position sell (local execution) ────────────────────────────

#[derive(Debug, Serialize)]
pub struct SellResult {
    pub signature: String,
    /// Raw base units actually sold (post on-chain clamp).
    pub sold_raw: String,
}

/// One unlocked Sol wallet's view for the sell-routing decision.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SellWallet {
    /// Base58 pubkey.
    address: String,
    primary: bool,
    /// On-chain balance of the mint being sold (raw base units).
    balance: u64,
}

/// Pick the wallet a manual sell must execute from (audit N2).
///
/// FAIL-CLOSED by design: when the holding wallet cannot be determined
/// unambiguously, refuse with a readable error instead of defaulting to
/// the primary — a wrong default mis-executes real money (sells a
/// fraction of the PRIMARY's holding, wrong wallet AND wrong size).
///
/// - `requested` (the UI's intents-ledger attribution) must name an
///   unlocked wallet that actually HOLDS the mint on-chain.
/// - With no attribution, exactly one on-chain holder is required;
///   zero or several holders refuse.
fn resolve_sell_wallet(
    candidates: &[SellWallet],
    requested: Option<&str>,
) -> Result<usize, String> {
    if candidates.is_empty() {
        return Err("no Solana wallet is unlocked on this device".into());
    }
    let holders: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.balance > 0)
        .map(|(i, _)| i)
        .collect();
    if let Some(req) = requested {
        let Some(i) = candidates.iter().position(|c| c.address == req) else {
            return Err(format!(
                "this position is attributed to wallet {} which is not unlocked in this \
                 vault — unlock that client to sell from it",
                short(req)
            ));
        };
        if candidates[i].balance == 0 {
            return Err(match holders.as_slice() {
                [] => format!(
                    "wallet {} holds none of this token on-chain (and no other unlocked \
                     wallet does either)",
                    short(req)
                ),
                [h] => format!(
                    "wallet {} holds none of this token — the on-chain holding sits in \
                     wallet {}; the sell was NOT re-routed automatically (stale \
                     attribution?)",
                    short(req),
                    short(&candidates[*h].address)
                ),
                _ => format!(
                    "wallet {} holds none of this token, while several other unlocked \
                     wallets do — refusing to guess",
                    short(req)
                ),
            });
        }
        return Ok(i);
    }
    match holders.as_slice() {
        [] => Err("no unlocked wallet holds this token on-chain".into()),
        [h] => Ok(*h),
        _ => Err(format!(
            "this token is held by {} unlocked wallets ({}) and the position has no \
             client attribution — refusing to pick one; sell from the web dashboard \
             or consolidate the holding first",
            holders.len(),
            holders
                .iter()
                .map(|&i| short(&candidates[i].address))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Sell `amount` base units of `mint` from the PRIMARY wallet through
/// the app's own `:5829` engine (`/quote` → `/swap`) — the daemon's
/// `SignerSlot` holds exactly the primary keypair.
async fn sell_via_local_daemon(
    state: &AppState,
    cfg: &SolConfig,
    mint: &str,
    amount: u64,
) -> Result<String, String> {
    // Auth into the daemon first — /swap 403s without a token.
    ensure_daemon_auth(state).await?;
    let base = daemon_base(state)?;
    let quote = daemon_post(
        &base,
        "/quote",
        &serde_json::json!({
            "inputMint": mint,
            "outputMint": WSOL,
            "amountLamports": amount,
            "slippageBps": cfg.slippage_bps,
        }),
    )
    .await?;
    let route_id = quote
        .get("routeId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "quote returned no routeId".to_string())?;
    let swap = daemon_post(
        &base,
        "/swap",
        &serde_json::json!({
            "routeId": route_id,
            "tipLamports": cfg.tip_lamports,
        }),
    )
    .await?;
    swap.get("txSignature")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "swap returned no signature".to_string())
}

/// Sell `amount` base units of `mint` from a SECONDARY wallet by
/// driving the same `BotEngine` the per-wallet runtime executors use
/// (`execute_sell`: native PumpFun/PumpSwap/Raydium routing with
/// Jupiter fallback, program allowlist, on-chain balance clamp, gateway
/// intent/fill reporting) — signed with THAT wallet's keypair. Sells
/// bypass matcher/budget by construction, so the zeroed budget here
/// guards nothing it shouldn't.
async fn sell_via_engine(
    state: &AppState,
    cfg: &SolConfig,
    rpc: &core::RpcClient,
    seed: &[u8; 32],
    mint: &str,
    amount: u64,
) -> Result<String, String> {
    use solana_sdk::signature::SeedDerivable as _;
    let kp = core::Keypair::from_seed(seed).map_err(|e| format!("keypair derive: {e}"))?;
    let auth = gateway::resolve_auth(state).await?;
    let sim_rpc_url = cfg.resolved_rpc_url_with_auth(Some((&auth.base, &auth.token)));
    let relay = core::RelayClient::new(auth.base, auth.token);
    let jup = core::JupiterClient::new();
    let allowlist = core::default_allowlist().map_err(|e| format!("allowlist: {e}"))?;
    let matcher = core::PresetMatcher {
        min_mcap_usd: None,
        max_mcap_usd: None,
        min_liquidity_usd: None,
        max_age_secs: None,
        blocked_tokens: Default::default(),
    };
    let budget = core::BudgetState::new(core::BudgetConfig {
        session_budget_lamports: 0,
        per_token_cap_lamports: None,
        per_hour_cap_lamports: None,
    });
    let bot_cfg = core::BotConfig {
        per_trade_lamports: 0, // sells size explicitly, never via config
        slippage_bps: cfg.slippage_bps,
        tip_lamports: cfg.tip_lamports,
        submit_mode: cfg.submit_mode.clone(),
        rpc_url: sim_rpc_url,
        skip_simulate: false,
        skip_allowlist: false,
        input_mint: WSOL.into(),
        pumpfun_cu_limit: 120_000,
        pumpfun_cu_price_micro_lamports: 50_000,
        bot_session_id: None,
        preset_id: None,
    };
    let mut engine = core::BotEngine::new(matcher, budget, allowlist, bot_cfg);
    match engine
        .execute_sell(mint.to_string(), amount, None, &jup, &relay, rpc, &kp)
        .await
    {
        Ok(core::Decision::Submitted(resp)) => resp
            .orders
            .into_iter()
            .find(|o| !o.signature.is_empty())
            .map(|o| o.signature)
            .ok_or_else(|| {
                format!(
                    "sell submitted (intent {}) but the relay returned no order signature",
                    resp.intent_id
                )
            }),
        Ok(core::Decision::Skipped(reason)) => Err(format!("sell skipped: {reason}")),
        Err(e) => Err(format!("sell failed: {e}")),
    }
}

/// Sell `fraction_bps` (1..=10000) of the HOLDING wallet's ON-CHAIN
/// balance of `mint` through this device's signer engine: native
/// PumpFun/PumpSwap/Raydium routing with Jupiter fallback, the program
/// allowlist, and gateway intent/fill reporting all included. 10000 bps
/// sells the full live balance — never a stale gateway `net_amount`.
///
/// Wallet routing (audit N2): `owner_pubkey` carries the UI's client
/// attribution; the actual on-chain holdings of EVERY unlocked Sol
/// wallet are probed and the sell executes from the wallet that holds
/// the position — primary via the `:5829` daemon, secondaries via a
/// direct engine run with their own keypair. Ambiguity refuses loudly
/// (see [`resolve_sell_wallet`]) instead of defaulting to the primary.
#[tauri::command]
pub async fn sol_position_sell(
    mint: String,
    fraction_bps: u32,
    owner_pubkey: Option<String>,
    state: State<'_, AppState>,
) -> Result<SellResult, String> {
    if !(1..=10_000).contains(&fraction_bps) {
        return Err("fraction must be 1..=10000 bps".into());
    }
    let mint = mint.trim().to_string();
    let mint_pk =
        solana_sdk::pubkey::Pubkey::from_str(&mint).map_err(|e| format!("mint invalid: {e}"))?;
    let requested = owner_pubkey
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Candidate wallets: every unlocked Sol vault client (seed in hand
    // for direct signing); legacy pre-vault installs fall back to the
    // single primary, which the `:5829` slot signs for.
    struct Candidate {
        address: String,
        primary: bool,
        seed: Option<[u8; 32]>,
    }
    let mut candidates: Vec<Candidate> = Vec::new();
    if let Ok(clients) = state.clients.lock() {
        for c in clients.iter() {
            if c.entry.chain != core::WalletChain::Sol {
                continue;
            }
            let Some(seed) = c.sol_seed else { continue };
            candidates.push(Candidate {
                address: c.entry.address.clone(),
                primary: c.role == crate::state::ClientRole::Primary,
                seed: Some(seed),
            });
        }
    }
    if candidates.is_empty() {
        if let Some(pk) = crate::clients::primary_sol_pubkey() {
            candidates.push(Candidate {
                address: pk,
                primary: true,
                seed: None,
            });
        }
    }
    if candidates.is_empty() {
        return Err("no Solana wallet set up yet".into());
    }

    let cfg = SolConfig::load_or_default();
    // Prefer the gateway RPC proxy when the user set no explicit RPC;
    // fall back to the legacy chain when no credentials resolve (the
    // balance probes still work on a public endpoint).
    let probe_auth = gateway::resolve_auth(&state).await.ok();
    let rpc = core::RpcClient::new(
        cfg.resolved_rpc_url_with_auth(
            probe_auth
                .as_ref()
                .map(|a| (a.base.as_str(), a.token.as_str())),
        ),
    );
    // Token-2022 mints live at a different ATA — resolve the mint's
    // owner program first (legacy SPL on lookup failure).
    let token_program = rpc
        .get_account_owner(&mint_pk)
        .await
        .ok()
        .flatten()
        .unwrap_or(core::dex::ata::TOKEN_PROGRAM_ID);

    // Probe every candidate's on-chain balance. FAIL-CLOSED: a probe
    // error refuses the sell — guessing the holder mis-executes money.
    let mut wallets: Vec<SellWallet> = Vec::with_capacity(candidates.len());
    for c in &candidates {
        let owner = solana_sdk::pubkey::Pubkey::from_str(&c.address)
            .map_err(|e| format!("wallet pubkey invalid ({}): {e}", short(&c.address)))?;
        let ata = core::dex::ata::derive_with_program(&owner, &mint_pk, &token_program);
        let balance = rpc
            .get_token_account_balance(&ata)
            .await
            .map_err(|e| format!("balance fetch for {}: {e}", short(&c.address)))?
            .unwrap_or(0);
        wallets.push(SellWallet {
            address: c.address.clone(),
            primary: c.primary,
            balance,
        });
    }

    let i = resolve_sell_wallet(&wallets, requested)?;
    let balance = wallets[i].balance;
    let amount = if fraction_bps == 10_000 {
        balance
    } else {
        ((balance as u128) * (fraction_bps as u128) / 10_000) as u64
    };
    if amount == 0 {
        return Err("computed sell amount is zero".into());
    }

    let signature = if wallets[i].primary {
        sell_via_local_daemon(&state, &cfg, &mint, amount).await?
    } else {
        let seed = candidates[i]
            .seed
            .as_ref()
            .ok_or_else(|| "holding wallet has no unlocked key on this device".to_string())?;
        sell_via_engine(&state, &cfg, &rpc, seed, &mint, amount).await?
    };
    Ok(SellResult {
        signature,
        sold_raw: amount.to_string(),
    })
}

// ─── Bot sessions: create / cancel / arm / disarm / device truth ────

/// Scanner preset (id + name) for the start-session form.
#[derive(Debug, Serialize, Deserialize)]
pub struct PresetLite {
    pub id: String,
    pub name: String,
}

#[tauri::command]
pub async fn alpha_presets(state: State<'_, AppState>) -> Result<Vec<PresetLite>, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(&auth, reqwest::Method::GET, "/api/alpha/presets", None).await?;
    parse(&text, "presets")
}

/// `POST /api/trading/bot/sessions` — the server budget row. Body is
/// the web's `CreateBotSessionReq` shape; returns the created row so
/// the caller can immediately arm this device against it.
#[tauri::command]
pub async fn bot_session_create(
    body: serde_json::Value,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::POST,
        "/api/trading/bot/sessions",
        Some(&body),
    )
    .await?;
    parse(&text, "bot session")
}

/// `DELETE /api/trading/bot/sessions/{id}` — cancel the server row.
/// 404 is treated as success (the daemon's best-effort cancel may have
/// already flipped it).
#[tauri::command]
pub async fn bot_session_cancel(
    session_id: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = gateway::resolve_auth(&state).await?;
    match gw(
        &auth,
        reqwest::Method::DELETE,
        &format!("/api/trading/bot/sessions/{session_id}"),
        None,
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.contains("404") => Ok(()),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct BotArmReq {
    pub session_id: String,
    pub preset_id: String,
    pub per_trade_lamports: i64,
    pub budget_lamports: i64,
    #[serde(default)]
    pub spent_lamports: i64,
    #[serde(default)]
    pub per_token_cap_lamports: Option<i64>,
    #[serde(default)]
    pub tip_lamports: Option<i64>,
}

/// Arm THIS device's in-process bot engine for a gateway session via
/// the daemon's `/bot/enable` (replaces any previously-armed session).
/// Arms with the REMAINING budget so a re-arm of a partially-spent
/// session can't overspend the server cap.
#[tauri::command]
pub async fn bot_arm(req: BotArmReq, state: State<'_, AppState>) -> Result<(), String> {
    if req.per_trade_lamports <= 0 {
        return Err("per-trade size must be > 0".into());
    }
    let remaining = (req.budget_lamports - req.spent_lamports).max(0);
    if remaining < req.per_trade_lamports {
        return Err(
            "remaining budget is below the per-trade size — nothing left to spend; \
             start a fresh session"
                .into(),
        );
    }
    ensure_daemon_auth(&state).await?;
    let base = daemon_base(&state)?;
    let cfg = SolConfig::load_or_default();
    daemon_post(
        &base,
        "/bot/enable",
        &serde_json::json!({
            "session_id": req.session_id,
            "preset_id": req.preset_id,
            "per_trade_lamports": req.per_trade_lamports,
            "session_budget_lamports": remaining,
            "per_token_lamports": req.per_token_cap_lamports,
            "slippage_bps": cfg.slippage_bps,
            "submit_mode": cfg.submit_mode,
            "tip_lamports": req.tip_lamports.unwrap_or(cfg.tip_lamports),
        }),
    )
    .await
    .map(|_| ())
}

/// Disarm the device's bot engine (`/bot/disable`). "No active bot
/// session" is success — the goal state is already true.
#[tauri::command]
pub async fn bot_disarm(
    session_id: Option<String>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let base = daemon_base(&state)?;
    match daemon_post(
        &base,
        "/bot/disable",
        &serde_json::json!({ "session_id": session_id }),
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.contains("no active bot session") => Ok(()),
        Err(e) => Err(e),
    }
}

/// Which gateway sessions THIS device's engine is armed for, straight
/// from the daemon's `/status` — the source of truth for the
/// "armed · this device" chip.
#[derive(Debug, Serialize)]
pub struct BotDeviceStatus {
    pub running: bool,
    pub unlocked: bool,
    pub armed_session_ids: Vec<String>,
}

#[tauri::command]
pub async fn bot_device_status(state: State<'_, AppState>) -> Result<BotDeviceStatus, String> {
    let base = match daemon_base(&state) {
        Ok(b) => b,
        Err(_) => {
            return Ok(BotDeviceStatus {
                running: false,
                unlocked: false,
                armed_session_ids: Vec::new(),
            })
        }
    };
    #[derive(Deserialize)]
    struct SessionInfo {
        #[serde(rename = "sessionId")]
        session_id: String,
    }
    #[derive(Deserialize)]
    struct DaemonStatus {
        connected: bool,
        #[serde(rename = "activeBotSessions", default)]
        active_bot_sessions: Vec<SessionInfo>,
    }
    let resp = http()
        .get(format!("{base}/status"))
        .send()
        .await
        .map_err(|e| format!("daemon /status: {e}"))?;
    let st: DaemonStatus = resp
        .json()
        .await
        .map_err(|e| format!("daemon /status decode: {e}"))?;
    Ok(BotDeviceStatus {
        running: true,
        unlocked: st.connected,
        armed_session_ids: st
            .active_bot_sessions
            .into_iter()
            .map(|s| s.session_id)
            .collect(),
    })
}

// ─── Hyperliquid position management ────────────────────────────────

/// `POST /api/hyperliquid/exchange/close` — reduce-only close /
/// partial reduce by percent of the LIVE position (signer-resolved at
/// execution time). Returns `{cloid, status}` (`status: "paper"` in
/// paper mode).
#[tauri::command]
pub async fn hl_close_position(
    coin: String,
    percent: f64,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    if !(percent > 0.0 && percent <= 100.0) {
        return Err("percent must be in (0, 100]".into());
    }
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::POST,
        "/api/hyperliquid/exchange/close",
        Some(&serde_json::json!({ "coin": coin, "percent": percent.to_string() })),
    )
    .await?;
    parse(&text, "close")
}

/// `POST /api/hyperliquid/exchange/tpsl` — attach reduce-only TP and/or
/// SL triggers to an existing position. Prices travel as decimal
/// strings (precision-safe); `close_percent` defaults to 100.
#[tauri::command]
pub async fn hl_place_tpsl(
    coin: String,
    tp_price_in: Option<String>,
    sl_price_in: Option<String>,
    close_percent: Option<f64>,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let tp = tp_price_in.filter(|s| !s.trim().is_empty());
    let sl = sl_price_in.filter(|s| !s.trim().is_empty());
    if tp.is_none() && sl.is_none() {
        return Err("set a TP price, an SL price, or both".into());
    }
    if let Some(p) = close_percent {
        if !(p > 0.0 && p <= 100.0) {
            return Err("close percent must be in (0, 100]".into());
        }
    }
    let auth = gateway::resolve_auth(&state).await?;
    let mut body = serde_json::json!({ "coin": coin });
    if let Some(tp) = tp {
        body["tp_price"] = serde_json::Value::String(tp.trim().to_string());
    }
    if let Some(sl) = sl {
        body["sl_price"] = serde_json::Value::String(sl.trim().to_string());
    }
    if let Some(p) = close_percent {
        body["close_percent"] = serde_json::Value::String(p.to_string());
    }
    let text = gw(
        &auth,
        reqwest::Method::POST,
        "/api/hyperliquid/exchange/tpsl",
        Some(&body),
    )
    .await?;
    parse(&text, "tpsl")
}

// ─── Hyperliquid copy-config editing ────────────────────────────────

/// Full HL copy configs, raw gateway JSON (the TS layer types the
/// fields — scale/max/min/mirror/leverage/drawdown/slippage/SL/TP/
/// retry/equity-basis).
#[tauri::command]
pub async fn hl_copy_configs_full(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::GET,
        "/api/hyperliquid/copy-trade/configs",
        None,
    )
    .await?;
    parse(&text, "hl copy configs")
}

/// `POST /api/hyperliquid/copy-trade/configs` — follow a new wallet.
/// Body is the web's `CopyTradeConfigInput` (target_wallet + knobs); a
/// 409 from the single-follow guard passes through verbatim.
#[tauri::command]
pub async fn hl_copy_config_create(
    body: serde_json::Value,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::POST,
        "/api/hyperliquid/copy-trade/configs",
        Some(&body),
    )
    .await?;
    parse(&text, "hl copy config")
}

/// `PATCH /api/hyperliquid/copy-trade/configs/{id}` — partial update;
/// also the enable/pause toggle (`{enabled}`). A 409
/// `already_following` passes through verbatim for the UI to explain.
#[tauri::command]
pub async fn hl_copy_config_update(
    config_id: String,
    patch: serde_json::Value,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let auth = gateway::resolve_auth(&state).await?;
    let text = gw(
        &auth,
        reqwest::Method::PATCH,
        &format!("/api/hyperliquid/copy-trade/configs/{config_id}"),
        Some(&patch),
    )
    .await?;
    parse(&text, "hl copy config")
}

// ─── Per-client preset-assignment overrides ─────────────────────────

/// `PUT /api/trading/clients/{id}/presets/{preset_id}` with the full
/// PATCH-semantics body (`enabled?`, `buy_size_lamports_override?`,
/// `ladder_override?`, `clear_*` flags) — the richer sibling of
/// `client_preset_assign`'s bare toggle.
#[tauri::command]
pub async fn client_preset_update(
    gateway_id: String,
    preset_id: String,
    body: serde_json::Value,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = gateway::resolve_auth(&state).await?;
    gw(
        &auth,
        reqwest::Method::PUT,
        &format!("/api/trading/clients/{gateway_id}/presets/{preset_id}"),
        Some(&body),
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_addr_truncates() {
        assert_eq!(short("abcdefghijk"), "abcd…hijk");
        assert_eq!(short("short"), "short");
    }

    // ── resolve_sell_wallet (audit N2) — fail-closed routing matrix ──

    fn w(address: &str, primary: bool, balance: u64) -> SellWallet {
        SellWallet {
            address: address.into(),
            primary,
            balance,
        }
    }

    #[test]
    fn sell_routes_to_the_single_holder_even_when_not_primary() {
        // The N2 bug: primary holds none, secondary holds the position.
        // The sell MUST route to the secondary, never the primary.
        let wallets = [w("PRIMARY", true, 0), w("SECONDARY", false, 500)];
        assert_eq!(resolve_sell_wallet(&wallets, None), Ok(1));
    }

    #[test]
    fn sell_with_matching_attribution_uses_that_wallet() {
        let wallets = [w("PRIMARY", true, 100), w("SECONDARY", false, 500)];
        assert_eq!(resolve_sell_wallet(&wallets, Some("SECONDARY")), Ok(1));
        assert_eq!(resolve_sell_wallet(&wallets, Some("PRIMARY")), Ok(0));
    }

    #[test]
    fn sell_refuses_when_multiple_holders_and_no_attribution() {
        // Fail-closed: never guess between two real holders.
        let wallets = [w("PRIMARY", true, 100), w("SECONDARY", false, 500)];
        let err = resolve_sell_wallet(&wallets, None).unwrap_err();
        assert!(err.contains("refusing to pick one"), "got: {err}");
    }

    #[test]
    fn sell_refuses_when_no_wallet_holds_the_mint() {
        let wallets = [w("PRIMARY", true, 0), w("SECONDARY", false, 0)];
        let err = resolve_sell_wallet(&wallets, None).unwrap_err();
        assert!(err.contains("no unlocked wallet holds"), "got: {err}");
    }

    #[test]
    fn sell_refuses_attribution_to_a_wallet_not_in_the_vault() {
        let wallets = [w("PRIMARY", true, 100)];
        let err = resolve_sell_wallet(&wallets, Some("ELSEWHERE1234")).unwrap_err();
        assert!(err.contains("not unlocked in this vault"), "got: {err}");
    }

    #[test]
    fn sell_refuses_stale_attribution_instead_of_rerouting() {
        // Attribution names the primary but the holding sits in the
        // secondary — refuse with the real holder named; NEVER silently
        // re-route (the user confirmed a different wallet).
        let wallets = [w("PRIMARY", true, 0), w("SECONDARY", false, 500)];
        let err = resolve_sell_wallet(&wallets, Some("PRIMARY")).unwrap_err();
        assert!(err.contains("NOT re-routed"), "got: {err}");
        assert!(err.contains("SECO"), "names the real holder: {err}");
    }

    #[test]
    fn sell_refuses_attributed_wallet_with_zero_balance_when_nobody_holds() {
        let wallets = [w("PRIMARY", true, 0), w("SECONDARY", false, 0)];
        let err = resolve_sell_wallet(&wallets, Some("PRIMARY")).unwrap_err();
        assert!(err.contains("holds none"), "got: {err}");
    }

    #[test]
    fn sell_refuses_with_no_unlocked_wallets() {
        let err = resolve_sell_wallet(&[], None).unwrap_err();
        assert!(err.contains("no Solana wallet"), "got: {err}");
    }
}
