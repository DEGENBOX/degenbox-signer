//! Localhost-only HTTP server exposing the frozen `signer-protocol`
//! RPC on `127.0.0.1:5829`. The web app probes `/health`; if it
//! answers, the page prefers the local desktop signer over the Chrome
//! extension. Canonical port of `signer-desktop/src/daemon.rs` (the
//! `signer-cli daemon` implementation) so the Tauri app serves the
//! exact same contract; the CLI keeps its own copy until deprecation.
//!
//! Differences from the CLI origin (host-shape, not wire-shape):
//!
//! - The signing keypair lives in a lockable [`SignerSlot`] instead of
//!   being decrypted once at boot — a GUI host starts the server at
//!   launch (so the web app detects it immediately) and installs /
//!   clears the keypair on unlock / lock. Endpoints that need the key
//!   answer `503 locked` until then; `/health` + `/quote` work always.
//! - `serve` surfaces a bind failure (port conflict) as a clear error
//!   for the host to display instead of process-exit.
//!
//! ## Security model
//!
//! - Binds **only** `127.0.0.1:5829`. Refusing 0.0.0.0 is the entire
//!   security story — the daemon runs in the user's session and trusts
//!   any localhost caller.
//! - **CORS** allowlist = the web app origins
//!   (`https://app.degenbox.io` + the dev hosts). Without that the
//!   browser refuses the cross-origin request entirely.
//! - Keystore stays encrypted on disk; the daemon prompts for the
//!   password ONCE at startup, holds the unlocked secret in memory,
//!   wipes on Ctrl-C / SIGTERM. The five-minute inactivity TTL the
//!   Chrome extension uses is overkill here because the daemon is a
//!   foreground process the user explicitly started.
//! - **Auth bridging**: the web app POSTs its JWT via `/setAuth` before
//!   the first `/swap`. Daemon stores it in-memory (NOT on disk) for
//!   relay calls.

use crate::dex::{
    pumpfun::{self, BondingCurveAccount},
    pumpfun_amm::{self, PoolAccount, PoolReserves},
    raydium_amm_v4::{
        self, AmmState as RaydiumAmm, MarketState as RaydiumMarket, PoolReserves as RaydiumReserves,
    },
};
use crate::{
    decode_jupiter_tx_b64, default_allowlist, route as signer_route, sign_jupiter_tx_b64,
    sign_versioned_tx_bytes, CreateIntentReq, JupiterClient, Keypair, QuoteResponse, RelayClient,
    RpcClient, Signer as _, SwapOptions, SwapRoute,
};
use crate::{
    spawn_priority_fee_poller, spawn_subscriber, AtaCache, BlockhashCache, BotConfig, BotEngine,
    BudgetConfig, BudgetState, Decision, FeeStrategy, FeeTier, PresetMatcher,
};
use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tower_http::cors::CorsLayer;

/// Default localhost port. Random-but-stable: chosen to be obscure
/// enough that nothing else commonly listens on it, low enough that
/// it doesn't need root, and recognisable.
const DEFAULT_PORT: u16 = 5829;

/// State for the currently-running bot session. At most one can be
/// active at a time; a new `/bot/enable` call replaces any prior one.
struct ActiveBot {
    session_id: String,
    preset_id: String,
    handle: JoinHandle<()>,
}

/// Lockable keypair slot. The host installs the unlocked keypair after
/// the user decrypts the keystore and clears it on lock — the HTTP
/// server itself runs for the whole app lifetime so the web app's
/// `/health` probe always answers.
///
/// `std::sync::RwLock` (not tokio) on purpose: every access is a
/// sub-microsecond pointer clone with no `.await` while held, and the
/// host's lock/unlock paths are synchronous IPC commands.
#[derive(Clone, Default)]
pub struct SignerSlot(Arc<std::sync::RwLock<Option<Arc<Keypair>>>>);

impl SignerSlot {
    /// Install (or replace) the unlocked keypair.
    pub fn install(&self, kp: Keypair) {
        let mut g = self.0.write().unwrap_or_else(|e| e.into_inner());
        *g = Some(Arc::new(kp));
    }
    /// Drop the keypair (lock). In-flight handlers holding an `Arc`
    /// clone finish their current request; new requests see locked.
    pub fn clear(&self) {
        let mut g = self.0.write().unwrap_or_else(|e| e.into_inner());
        *g = None;
    }
    /// Current keypair, or `None` while locked.
    pub fn unlocked(&self) -> Option<Arc<Keypair>> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[derive(Clone)]
pub struct DaemonState {
    /// The lockable signing keypair (see [`SignerSlot`]).
    pub kp: SignerSlot,
    /// Gateway base URL + JWT (set via `setAuth`). Token is Optional
    /// because connect/quote/status work without it; swap/bot do not.
    pub config: Arc<RwLock<RuntimeConfig>>,
    /// Jupiter client — direct, NEVER through the gateway. Trust model
    /// mirrors the extension: the gateway is the race-submit fan-out,
    /// nothing more.
    pub jup: Arc<JupiterClient>,
    /// Solana RPC client used by the PumpFun-native swap path —
    /// `getAccountInfo` for the BondingCurve PDA at quote-time +
    /// `getLatestBlockhash` at swap-time.
    pub rpc: Arc<RpcClient>,
    /// Background-refreshed blockhash cache shared across all bot
    /// sessions. Spawned once at daemon boot; reading the current
    /// blockhash is a lock-acquire instead of an RPC round-trip,
    /// saving 30–100 ms per trade.
    pub blockhash_cache: BlockhashCache,
    /// Shared `(owner, mint)` → ATA-exists cache. Persists across
    /// bot sessions for the daemon lifetime so a single wallet gets
    /// "warmer" the more it trades — repeat buys on familiar mints
    /// drop the `CreateIdempotent` instruction entirely.
    pub ata_cache: AtaCache,
    /// Shared fee strategy: per-DEX CU limits + global priority-fee
    /// tiers. A background poller (when `HELIUS_RPC_URL` is set)
    /// keeps the tiers updated from the network's current
    /// `getPriorityFeeEstimate` distribution.
    pub fee_strategy: FeeStrategy,
    /// Raw RPC URL — kept alongside `rpc` so the bot engine can pass
    /// it into `BotConfig::rpc_url` for the pre-sign simulator without
    /// going through the `RpcClient` internals.
    pub rpc_url: String,
    /// Per-route id → cached quote. Same TTL semantics as the extension
    /// (~30s); not persisted across process restarts.
    pub routes: Arc<RwLock<RouteCache>>,
    /// Currently-armed bot session + its background task. Protected by
    /// a Mutex so both `/bot/enable` and `/bot/disable` can safely
    /// swap the slot. At most one active at a time.
    // Private — all bot-lifecycle handlers live in the same module.
    active_bot: Arc<Mutex<Option<ActiveBot>>>,
    /// `clientKind` reported on `/status` so the web app can tell which
    /// host serves the daemon. Defaults to `"signer-app"` (the original
    /// host); other hosts override via [`DaemonState::with_client_kind`].
    client_kind: &'static str,
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub gateway_base: String,
    pub auth_token: Option<String>,
}

#[derive(Debug, Default)]
pub struct RouteCache {
    inner: std::collections::HashMap<String, CachedRoute>,
}

/// Which engine builds the unsigned tx at swap-time.
///
/// `Jupiter` carries the verbatim aggregator quote so the swap path
/// can replay it losslessly (we don't re-quote at swap-time — would
/// race the user against price drift between confirm + sign).
///
/// `PumpFun` carries a snapshot of the live BondingCurve account
/// taken at quote-time + which direction the swap goes; the swap
/// path uses these to call `pumpfun::build_buy_tx` /
/// `build_sell_tx` directly without going through any aggregator.
#[derive(Debug, Clone)]
// The Raydium variant is larger than Jupiter/PumpFun because it carries
// AmmState + MarketState inline. CachedRouteKind is always stored inside
// CachedRoute which lives heap-allocated in the route HashMap — no
// stack-size concern in practice.
#[allow(clippy::large_enum_variant)]
pub enum CachedRouteKind {
    Jupiter(QuoteResponse),
    PumpFun {
        curve: BondingCurveAccount,
        /// Owner program of the mint (legacy SPL Token or Token-2022).
        token_program: solana_sdk::pubkey::Pubkey,
        side: PumpFunSide,
    },
    /// Post-graduation Pumpswap pool. Carries the resolved pool
    /// pubkey + decoded pool state + live base/quote vault balances
    /// — same lossless-replay contract as `PumpFun` so the swap
    /// path doesn't re-quote.
    PumpFunAmm {
        pool_pubkey: solana_sdk::pubkey::Pubkey,
        pool: PoolAccount,
        reserves: PoolReserves,
        /// Owner program of `pool.base_mint`.
        base_token_program: solana_sdk::pubkey::Pubkey,
        /// Owner program of `pool.quote_mint` (WSOL = legacy).
        quote_token_program: solana_sdk::pubkey::Pubkey,
        side: PumpFunSide,
    },
    /// Raydium AMM v4 pool. Carries the decoded AMM state + OpenBook
    /// market state + live coin/pc vault reserves. Same lossless-replay
    /// contract — swap handler builds tx from cache without re-fetching.
    Raydium {
        amm_pubkey: solana_sdk::pubkey::Pubkey,
        amm: RaydiumAmm,
        market: RaydiumMarket,
        reserves: RaydiumReserves,
        side: PumpFunSide,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpFunSide {
    /// SOL → token (input_mint == WSOL).
    Buy,
    /// token → SOL (output_mint == WSOL).
    Sell,
}

#[derive(Debug, Clone)]
pub struct CachedRoute {
    /// How this route was constructed + what data the swap path needs
    /// to rebuild the unsigned tx.
    pub kind: CachedRouteKind,
    pub input_mint: String,
    pub output_mint: String,
    pub amount_lamports: u64,
    /// Kept for parity with extension's CachedRoute shape + future
    /// UI surfaces ("show me the expected-out the user committed to"
    /// in the audit log). Currently read only by tests.
    #[allow(dead_code)]
    pub expected_out_lamports: u64,
    /// The floor the quote promised. For Jupiter routes this IS the
    /// swap-time floor (the quote is replayed verbatim); native routes
    /// recompute from reserves + the effective slippage at swap-time —
    /// see [`route_effective_min_out`].
    pub min_out_lamports: u64,
    pub slippage_bps: u16,
    pub expires_at_unix_ms: u64,
}

impl RouteCache {
    fn insert(&mut self, id: String, route: CachedRoute) {
        // Janitor: prune at most a handful on each insert.
        if self.inner.len() > 32 {
            let now = now_ms();
            self.inner.retain(|_, r| r.expires_at_unix_ms > now);
        }
        self.inner.insert(id, route);
    }
    fn consume(&mut self, id: &str) -> Option<CachedRoute> {
        let r = self.inner.remove(id)?;
        if r.expires_at_unix_ms < now_ms() {
            None
        } else {
            Some(r)
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Build + serve. Blocks until the process gets a stop signal.
pub async fn serve(state: DaemonState, port: u16) -> Result<()> {
    let allowed_origins: Vec<HeaderValue> = [
        // v2 frontend (staging today, apex on cutover).
        "https://staging.degenbox.app",
        "https://degenbox.app",
        "https://www.degenbox.app",
        // Legacy origins the CLI daemon allowed — kept for parity.
        "https://app.degenbox.io",
        "https://degenbox.io",
        // Local dev hosts (vite + docker frontend).
        "http://localhost:5173",
        "http://localhost:3000",
        "http://localhost:8091",
        "http://127.0.0.1:5173",
        "http://127.0.0.1:8091",
    ]
    .iter()
    .filter_map(|s| HeaderValue::from_str(s).ok())
    .collect();

    let cors = CorsLayer::new()
        .allow_origin(allowed_origins)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
        ])
        // Chrome's Private Network Access: a public HTTPS page (the
        // dashboard) may only reach 127.0.0.1 when the preflight is
        // answered with `Access-Control-Allow-Private-Network: true`.
        // Without it, current Chrome silently blocks even the no-cors
        // /health probe and the dashboard shows "Signer not detected"
        // while the app is fully connected. Origin allowlist above
        // still gates who gets it.
        .allow_private_network(true);

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/connect", get(connect))
        .route("/status", get(status))
        .route("/quote", post(quote))
        .route("/swap", post(swap))
        .route("/setAuth", post(set_auth))
        .route("/setGateway", post(set_gateway))
        .route("/bot/enable", post(bot_enable))
        .route("/bot/disable", post(bot_disable))
        .with_state(state)
        .layer(cors);

    // 127.0.0.1 ONLY — never bind 0.0.0.0. Public exposure of this
    // port would let anyone on the LAN sign-as-user.
    let addr: SocketAddr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!(
            "bind {addr} failed — port {port} already in use? Another DegenBox \
             signer (signer-cli daemon or a second app instance) may be running"
        )
    })?;
    tracing::info!(%addr, "signer-protocol daemon listening");
    axum::serve(listener, app).await.context("serve")?;
    Ok(())
}

impl DaemonState {
    /// Build the daemon state around a (possibly still-locked) signer
    /// slot. Must run inside a tokio runtime — spawns the shared
    /// blockhash refresher BEFORE serving so the first trade after boot
    /// can already hit a warm cache.
    pub fn new(kp: SignerSlot, gateway: String, rpc_url: String) -> Self {
        let rpc_client = RpcClient::new(rpc_url.clone());
        let blockhash_cache = BlockhashCache::new(rpc_client.clone());
        let ata_cache = AtaCache::new();
        let fee_strategy = FeeStrategy::new();
        // Best-effort poller: if HELIUS_RPC_URL is set, the strategy's
        // priority-fee tiers get updated every 2 s from
        // `getPriorityFeeEstimate`. When unset, static defaults are used
        // and the poller exits immediately.
        let helius_url = std::env::var("HELIUS_RPC_URL").unwrap_or_default();
        spawn_priority_fee_poller(fee_strategy.clone(), helius_url);
        DaemonState {
            kp,
            config: Arc::new(RwLock::new(RuntimeConfig {
                gateway_base: gateway,
                auth_token: None,
            })),
            jup: Arc::new(JupiterClient::new()),
            rpc: Arc::new(rpc_client),
            rpc_url,
            routes: Arc::new(RwLock::new(RouteCache::default())),
            active_bot: Arc::new(Mutex::new(None)),
            blockhash_cache,
            ata_cache,
            fee_strategy,
            client_kind: "signer-app",
        }
    }

    /// Override the `clientKind` reported on `/status` (additive —
    /// hosts other than the Tauri app identify themselves truthfully).
    pub fn with_client_kind(mut self, kind: &'static str) -> Self {
        self.client_kind = kind;
        self
    }
}

pub fn default_port() -> u16 {
    DEFAULT_PORT
}

// ─── handlers ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ConnectResult {
    pubkey: String,
    #[serde(rename = "balanceLamports")]
    balance_lamports: u64,
    #[serde(skip_serializing_if = "Option::is_none", rename = "walletLabel")]
    wallet_label: Option<String>,
}

async fn connect(State(s): State<DaemonState>) -> Result<Json<ConnectResult>, AppError> {
    let kp = s.kp.unlocked().ok_or_else(AppError::locked)?;
    Ok(Json(ConnectResult {
        pubkey: kp.pubkey().to_string(),
        // The daemon doesn't poll RPC for balance; the frontend asks
        // the gateway directly. Zero is the honest answer here — we
        // don't pretend to know.
        balance_lamports: 0,
        wallet_label: None,
    }))
}

/// Per-session info surfaced by `/status`. Matches the shape that the
/// web app expects from the signer-protocol `status` response.
#[derive(Debug, Serialize)]
struct ActiveSessionInfo {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "presetId")]
    preset_id: String,
}

#[derive(Debug, Serialize)]
struct SignerStatus {
    connected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pubkey: Option<String>,
    #[serde(rename = "activeBotSessions")]
    active_bot_sessions: Vec<ActiveSessionInfo>,
    /// Additive (the web app tolerates unknown fields): lets it
    /// distinguish client generations without a new endpoint.
    version: &'static str,
    #[serde(rename = "clientKind")]
    client_kind: &'static str,
}

async fn status(State(s): State<DaemonState>) -> Json<SignerStatus> {
    let guard = s.active_bot.lock().await;
    let sessions = match guard.as_ref() {
        Some(bot) if !bot.handle.is_finished() => vec![ActiveSessionInfo {
            session_id: bot.session_id.clone(),
            preset_id: bot.preset_id.clone(),
        }],
        _ => vec![],
    };
    let pubkey = s.kp.unlocked().map(|kp| kp.pubkey().to_string());
    Json(SignerStatus {
        connected: pubkey.is_some(),
        pubkey,
        active_bot_sessions: sessions,
        version: crate::VERSION,
        client_kind: s.client_kind,
    })
}

#[derive(Debug, Deserialize)]
struct QuoteRequest {
    #[serde(rename = "inputMint")]
    input_mint: String,
    #[serde(rename = "outputMint")]
    output_mint: String,
    #[serde(rename = "amountLamports")]
    amount_lamports: u64,
    #[serde(rename = "slippageBps", default)]
    slippage_bps: Option<u16>,
    /// Optional Raydium AMM v4 pool address. When present and the token
    /// is identified as a Raydium token, the daemon skips the PDA-based
    /// PumpFun discovery steps and resolves the route via the AMM hint.
    /// Passed through to `select_for_token_with_hint`.
    #[serde(rename = "ammHint", default)]
    amm_hint: Option<String>,
}

#[derive(Debug, Serialize)]
struct QuoteResult {
    #[serde(rename = "routeId")]
    route_id: String,
    #[serde(rename = "expectedOutLamports")]
    expected_out_lamports: u64,
    #[serde(rename = "minOutLamports")]
    min_out_lamports: u64,
    #[serde(rename = "priceImpactPct")]
    price_impact_pct: f64,
    #[serde(rename = "feeLamports")]
    fee_lamports: u64,
}

async fn quote(
    State(s): State<DaemonState>,
    Json(req): Json<QuoteRequest>,
) -> Result<Json<QuoteResult>, AppError> {
    let slip = req.slippage_bps.unwrap_or(500);
    let sol_mint = "So11111111111111111111111111111111111111112";
    let is_buy = req.input_mint == sol_mint;
    let is_sell = req.output_mint == sol_mint;
    // Bonding-curve dispatch is only sensible for SOL↔token; if both
    // sides are non-SOL we don't try to be clever — Jupiter handles it.
    if is_buy || is_sell {
        let token_mint = if is_buy {
            &req.output_mint
        } else {
            &req.input_mint
        };
        match signer_route::select_for_token_with_hint(token_mint, req.amm_hint.as_deref(), &s.rpc)
            .await
        {
            Ok(SwapRoute::PumpFun(r)) => {
                let side = if is_buy {
                    PumpFunSide::Buy
                } else {
                    PumpFunSide::Sell
                };
                let (expected, min_out) =
                    pumpfun_quote_amounts(&r.curve, &side, req.amount_lamports, slip).ok_or_else(
                        || {
                            AppError::bad_request(
                                "pumpfun quote overflow — curve reserves invalid".to_string(),
                            )
                        },
                    )?;
                let id = uuid_v4_string();
                let route = CachedRoute {
                    kind: CachedRouteKind::PumpFun {
                        curve: r.curve,
                        token_program: r.token_program,
                        side,
                    },
                    input_mint: req.input_mint,
                    output_mint: req.output_mint,
                    amount_lamports: req.amount_lamports,
                    expected_out_lamports: expected,
                    min_out_lamports: min_out,
                    slippage_bps: slip,
                    expires_at_unix_ms: now_ms() + 30_000,
                };
                s.routes.write().await.insert(id.clone(), route);
                return Ok(Json(QuoteResult {
                    route_id: id,
                    expected_out_lamports: expected,
                    min_out_lamports: min_out,
                    // Native AMM has no aggregator-derived price impact;
                    // surface 0 (not a meaningful number for a single-pool
                    // constant-product trade — caller infers from slippage).
                    price_impact_pct: 0.0,
                    fee_lamports: 0,
                }));
            }
            Ok(SwapRoute::PumpFunAmm(amm)) => {
                let side = if is_buy {
                    PumpFunSide::Buy
                } else {
                    PumpFunSide::Sell
                };
                let (expected, min_out) =
                    pumpfun_amm_quote_amounts(&amm.reserves, &side, req.amount_lamports, slip)
                        .ok_or_else(|| {
                            AppError::bad_request(
                                "pumpfun-amm quote overflow — pool reserves invalid".to_string(),
                            )
                        })?;
                let id = uuid_v4_string();
                let route = CachedRoute {
                    kind: CachedRouteKind::PumpFunAmm {
                        pool_pubkey: amm.pool_pubkey,
                        pool: amm.pool,
                        reserves: amm.reserves,
                        base_token_program: amm.base_token_program,
                        quote_token_program: amm.quote_token_program,
                        side,
                    },
                    input_mint: req.input_mint,
                    output_mint: req.output_mint,
                    amount_lamports: req.amount_lamports,
                    expected_out_lamports: expected,
                    min_out_lamports: min_out,
                    slippage_bps: slip,
                    expires_at_unix_ms: now_ms() + 30_000,
                };
                s.routes.write().await.insert(id.clone(), route);
                return Ok(Json(QuoteResult {
                    route_id: id,
                    expected_out_lamports: expected,
                    min_out_lamports: min_out,
                    price_impact_pct: 0.0,
                    fee_lamports: 0,
                }));
            }
            Ok(SwapRoute::Raydium(r)) => {
                let side = if is_buy {
                    PumpFunSide::Buy
                } else {
                    PumpFunSide::Sell
                };
                let (expected, min_out) =
                    raydium_quote_amounts(&r.amm, &r.reserves, &side, req.amount_lamports, slip)
                        .ok_or_else(|| {
                            AppError::bad_request(
                                "raydium quote overflow — pool reserves invalid".to_string(),
                            )
                        })?;
                let id = uuid_v4_string();
                let route = CachedRoute {
                    kind: CachedRouteKind::Raydium {
                        amm_pubkey: r.amm_pubkey,
                        amm: r.amm,
                        market: r.market,
                        reserves: r.reserves,
                        side,
                    },
                    input_mint: req.input_mint,
                    output_mint: req.output_mint,
                    amount_lamports: req.amount_lamports,
                    expected_out_lamports: expected,
                    min_out_lamports: min_out,
                    slippage_bps: slip,
                    expires_at_unix_ms: now_ms() + 30_000,
                };
                s.routes.write().await.insert(id.clone(), route);
                return Ok(Json(QuoteResult {
                    route_id: id,
                    expected_out_lamports: expected,
                    min_out_lamports: min_out,
                    price_impact_pct: 0.0,
                    fee_lamports: 0,
                }));
            }
            // Either no native route (Jupiter handles it) or RPC
            // failure (degrade to Jupiter so the user can still trade
            // — losing route info is a smaller harm than a 502).
            Ok(SwapRoute::Jupiter) | Err(_) => {}
        }
    }

    // Default + fallback path: Jupiter aggregator.
    let q = s
        .jup
        .quote(&req.input_mint, &req.output_mint, req.amount_lamports, slip)
        .await
        .map_err(|e| AppError::bad_gateway(format!("jupiter quote: {e}")))?;
    let id = uuid_v4_string();
    let expected = q.out_amount.parse::<u64>().unwrap_or(0);
    let min_out = q.other_amount_threshold.parse::<u64>().unwrap_or(0);
    // Jupiter's priceImpactPct is a fraction ("0.05" = 5%). Emit a true
    // percent so the web UI's impact-vs-slippage gate (pct * 100 → bps) and
    // displays match the backend's normalized quote. The /swap round-trip
    // uses the untouched raw `q`, so scaling this display value is safe.
    let price_impact = q.price_impact_pct.parse::<f64>().unwrap_or(0.0) * 100.0;
    let route = CachedRoute {
        kind: CachedRouteKind::Jupiter(q),
        input_mint: req.input_mint,
        output_mint: req.output_mint,
        amount_lamports: req.amount_lamports,
        expected_out_lamports: expected,
        min_out_lamports: min_out,
        slippage_bps: slip,
        expires_at_unix_ms: now_ms() + 30_000,
    };
    s.routes.write().await.insert(id.clone(), route);
    Ok(Json(QuoteResult {
        route_id: id,
        expected_out_lamports: expected,
        min_out_lamports: min_out,
        price_impact_pct: price_impact,
        fee_lamports: 0,
    }))
}

/// Pure helper: compute `(expected_out, min_out_after_slippage)` for a
/// PumpFun swap against the supplied curve. Returns `None` on integer
/// overflow / zero reserves. Extracted so unit tests can exercise it
/// without spinning up the daemon.
pub(crate) fn pumpfun_quote_amounts(
    curve: &BondingCurveAccount,
    side: &PumpFunSide,
    amount_in: u64,
    slippage_bps: u16,
) -> Option<(u64, u64)> {
    match side {
        PumpFunSide::Buy => {
            let expected = pumpfun::buy_quote(
                amount_in,
                curve.virtual_sol_reserves,
                curve.virtual_token_reserves,
            )?;
            // Buys: slippage applies to TOKEN output (we want at LEAST
            // some amount, so deflate).
            let min_out = pumpfun::apply_slippage(expected, slippage_bps, false);
            Some((expected, min_out))
        }
        PumpFunSide::Sell => {
            let expected = pumpfun::sell_quote(
                amount_in,
                curve.virtual_sol_reserves,
                curve.virtual_token_reserves,
            )?;
            // Sells: slippage applies to SOL output (deflate).
            let min_out = pumpfun::apply_slippage(expected, slippage_bps, false);
            Some((expected, min_out))
        }
    }
}

/// Same shape as `pumpfun_quote_amounts` but for the post-graduation
/// Pumpswap pool. The pool's vault reserves come from the route
/// resolver; both sides apply slippage on the *output* side only
/// (caller-friendly: the quote tells the user how much they'll
/// minimum-receive, which is what the on-chain bound checks).
pub(crate) fn pumpfun_amm_quote_amounts(
    reserves: &PoolReserves,
    side: &PumpFunSide,
    amount_in: u64,
    slippage_bps: u16,
) -> Option<(u64, u64)> {
    match side {
        PumpFunSide::Buy => {
            // Buy: quote-in (SOL) → base-out (token).
            let expected = pumpfun_amm::buy_quote(amount_in, reserves.base, reserves.quote)?;
            let min_out = pumpfun_amm::apply_slippage(expected, slippage_bps, false);
            Some((expected, min_out))
        }
        PumpFunSide::Sell => {
            // Sell: base-in (token) → quote-out (SOL).
            let expected = pumpfun_amm::sell_quote(amount_in, reserves.base, reserves.quote)?;
            let min_out = pumpfun_amm::apply_slippage(expected, slippage_bps, false);
            Some((expected, min_out))
        }
    }
}

/// Quote amounts for a Raydium AMM v4 swap. Uses the AMM's on-chain
/// fee config (`swap_fee_numerator / swap_fee_denominator`, canonical =
/// 25/10000 = 0.25%). Slippage applied to the output side only — same
/// convention as PumpSwap helper above.
///
/// Buy:  pc-in (WSOL) → coin-out (token).  reserve_in=pc, reserve_out=coin.
/// Sell: coin-in (token) → pc-out (WSOL).  reserve_in=coin, reserve_out=pc.
pub(crate) fn raydium_quote_amounts(
    amm: &RaydiumAmm,
    reserves: &RaydiumReserves,
    side: &PumpFunSide,
    amount_in: u64,
    slippage_bps: u16,
) -> Option<(u64, u64)> {
    match side {
        PumpFunSide::Buy => {
            let expected = raydium_amm_v4::swap_base_in_quote(
                amount_in,
                reserves.pc,
                reserves.coin,
                amm.swap_fee_numerator,
                amm.swap_fee_denominator,
            )?;
            let min_out = raydium_amm_v4::apply_slippage(expected, slippage_bps, false);
            Some((expected, min_out))
        }
        PumpFunSide::Sell => {
            let expected = raydium_amm_v4::swap_base_in_quote(
                amount_in,
                reserves.coin,
                reserves.pc,
                amm.swap_fee_numerator,
                amm.swap_fee_denominator,
            )?;
            let min_out = raydium_amm_v4::apply_slippage(expected, slippage_bps, false);
            Some((expected, min_out))
        }
    }
}

#[derive(Debug, Deserialize)]
struct SwapRequest {
    #[serde(rename = "routeId")]
    route_id: String,
    #[serde(rename = "slippageBps", default)]
    slippage_bps: Option<u16>,
    #[serde(rename = "tipLamports", default)]
    tip_lamports: Option<i64>,
    /// When the web UI pre-created the intent (so it could render a
    /// pending state), it passes that id here and the daemon skips
    /// `create_intent` to avoid orphaning the first row in `pending`.
    #[serde(rename = "intentId", default)]
    intent_id: Option<String>,
    /// Hard floor on the output amount (raw base units) — the min-out
    /// the user actually saw on the web quote card
    /// (`signerProbe.ts` sends it on every QuickBuy/QuickSell). The
    /// daemon derives its own route, so without honouring this field
    /// the previewed floor guaranteed nothing. When the route's
    /// effective min-out falls below the floor the swap is refused with
    /// HTTP 409 ("re-quote") — never silently proceeded.
    #[serde(rename = "minOutLamports", default)]
    min_out_lamports: Option<u64>,
}

#[derive(Debug, Serialize)]
struct SwapResult {
    #[serde(rename = "txSignature")]
    tx_signature: String,
}

/// The min-out the transaction built from this route will actually
/// enforce on-chain, under `slippage_bps`. Jupiter routes replay the
/// cached quote verbatim (the per-request slippage override does not
/// re-quote), so their floor IS the cached one; native routes recompute
/// from the cached reserves exactly like the tx builders do. `None` =
/// reserve math overflowed (invalid reserves). Pure for unit testing.
pub(crate) fn route_effective_min_out(
    kind: &CachedRouteKind,
    cached_min_out: u64,
    amount_in: u64,
    slippage_bps: u16,
) -> Option<u64> {
    match kind {
        CachedRouteKind::Jupiter(_) => Some(cached_min_out),
        CachedRouteKind::PumpFun { curve, side, .. } => {
            pumpfun_quote_amounts(curve, side, amount_in, slippage_bps).map(|(_, m)| m)
        }
        CachedRouteKind::PumpFunAmm { reserves, side, .. } => {
            pumpfun_amm_quote_amounts(reserves, side, amount_in, slippage_bps).map(|(_, m)| m)
        }
        CachedRouteKind::Raydium {
            amm,
            reserves,
            side,
            ..
        } => raydium_quote_amounts(amm, reserves, side, amount_in, slippage_bps).map(|(_, m)| m),
    }
}

async fn swap(
    State(s): State<DaemonState>,
    Json(req): Json<SwapRequest>,
) -> Result<Json<SwapResult>, AppError> {
    let cfg = s.config.read().await.clone();
    let token = cfg
        .auth_token
        .ok_or_else(|| AppError::forbidden("no auth token — call /setAuth first"))?;
    let kp = s.kp.unlocked().ok_or_else(AppError::locked)?;

    let route = s
        .routes
        .write()
        .await
        .consume(&req.route_id)
        .ok_or_else(|| AppError::bad_request("route expired — re-quote"))?;

    let allow = default_allowlist().map_err(|e| AppError::internal(format!("allowlist: {e}")))?;
    let sol_mint = "So11111111111111111111111111111111111111112";
    let slippage_bps = req.slippage_bps.unwrap_or(route.slippage_bps);
    let tip_lamports = req.tip_lamports.unwrap_or(1_000_000);

    // M17: enforce the caller's previewed min-out floor BEFORE building
    // or signing anything. If the tx we'd build enforces a weaker floor
    // than the one the user confirmed, refuse loudly (409) so the FE
    // re-quotes — silently proceeding would make the previewed
    // guarantee a fiction.
    if let Some(floor) = req.min_out_lamports {
        let effective = route_effective_min_out(
            &route.kind,
            route.min_out_lamports,
            route.amount_lamports,
            slippage_bps,
        )
        .ok_or_else(|| AppError::bad_request("quote overflow — pool reserves invalid"))?;
        if effective < floor {
            return Err(AppError::conflict(format!(
                "min-out floor not met: this route enforces at least {effective} but you \
                 confirmed a minimum of {floor} — re-quote and retry"
            )));
        }
    }

    // Build + sign the unsigned tx according to the cached route kind.
    // Both branches converge to `(signed_b64, intent_side, quote_snapshot,
    // atas_to_mark)` so the gateway-relay logic below is shared.
    //
    // `atas_to_mark` is the list of mints whose user-owned ATA we'll
    // record as known in `s.ata_cache` AFTER a successful submit, so
    // future trades on the same wallet skip the redundant
    // `CreateIdempotent` instruction (~3.5k CU + ~120 bytes saved).
    let (signed_b64, intent_side, quote_snapshot, atas_to_mark) = match &route.kind {
        CachedRouteKind::Jupiter(jup_quote) => {
            let our_pub = kp.pubkey().to_string();
            let swap = s
                .jup
                .swap(
                    jup_quote,
                    &our_pub,
                    SwapOptions {
                        wrap_unwrap_sol: true,
                        priority_fee_lamports: None,
                    },
                )
                .await
                .map_err(|e| AppError::bad_gateway(format!("jupiter swap: {e}")))?;
            let decoded = decode_jupiter_tx_b64(&swap.swap_transaction)
                .map_err(|e| AppError::bad_request(format!("decode tx: {e}")))?;
            allow
                .check_tx(&decoded)
                .map_err(|e| AppError::bad_request(format!("program allowlist: {e}")))?;
            let signed = sign_jupiter_tx_b64(&swap.swap_transaction, kp.as_ref())
                .map_err(|e| AppError::bad_request(format!("sign: {e}")))?;
            let is_buy = route.input_mint == sol_mint;
            let side = if is_buy { "buy" } else { "sell" };
            let snap = serde_json::to_value(jup_quote).ok();
            // Jupiter builds its own ATA instructions inside the
            // returned tx; we don't know which mints it touches from
            // here without decoding, so leave the cache alone.
            (signed, side, snap, Vec::<Pubkey>::new())
        }
        CachedRouteKind::PumpFun {
            curve,
            token_program,
            side,
        } => {
            // Token mint = whichever side is NOT WSOL. The PumpFun
            // builder always takes the token mint explicitly + figures
            // out user vs vault ATAs from PDA derivation.
            let token_mint = match side {
                PumpFunSide::Buy => &route.output_mint,
                PumpFunSide::Sell => &route.input_mint,
            };
            let mint_pk = Pubkey::from_str(token_mint)
                .map_err(|_| AppError::bad_request(format!("invalid token mint: {token_mint}")))?;
            let user = kp.pubkey();
            // Fresh blockhash, fetched at swap-time (not quote-time)
            // so we don't risk a stale-hash reject for slow signers.
            let blockhash = s
                .blockhash_cache
                .get()
                .await
                .map_err(|e| AppError::bad_gateway(format!("blockhash: {e}")))?;
            let unsigned_bytes = match side {
                PumpFunSide::Buy => {
                    let params = pumpfun::BuyTxParams {
                        user,
                        mint: mint_pk,
                        creator: curve.creator,
                        token_program: *token_program,
                        sol_in_lamports: route.amount_lamports,
                        slippage_bps,
                        virtual_sol_reserves: curve.virtual_sol_reserves,
                        virtual_token_reserves: curve.virtual_token_reserves,
                        recent_blockhash: blockhash,
                        compute_unit_limit: 120_000,
                        compute_unit_price_micro_lamports: 50_000,
                        skip_token_ata_create: s.ata_cache.is_known(&user, &mint_pk),
                    };
                    pumpfun::build_buy_tx(&params)
                        .map_err(|e| AppError::bad_request(format!("build_buy_tx: {e}")))?
                }
                PumpFunSide::Sell => {
                    let params = pumpfun::SellTxParams {
                        user,
                        mint: mint_pk,
                        creator: curve.creator,
                        token_program: *token_program,
                        token_in_amount: route.amount_lamports,
                        slippage_bps,
                        virtual_sol_reserves: curve.virtual_sol_reserves,
                        virtual_token_reserves: curve.virtual_token_reserves,
                        recent_blockhash: blockhash,
                        compute_unit_limit: 100_000,
                        compute_unit_price_micro_lamports: 50_000,
                    };
                    pumpfun::build_sell_tx(&params)
                        .map_err(|e| AppError::bad_request(format!("build_sell_tx: {e}")))?
                }
            };
            // Allowlist on the pre-sign tx — PumpFun + ComputeBudget +
            // ATA + Token + System are all in DEFAULT_ALLOWED.
            let tx: solana_sdk::transaction::VersionedTransaction =
                bincode::deserialize(&unsigned_bytes)
                    .map_err(|e| AppError::bad_request(format!("decode tx: {e}")))?;
            allow
                .check_tx(&tx)
                .map_err(|e| AppError::bad_request(format!("program allowlist: {e}")))?;
            let signed_bytes = sign_versioned_tx_bytes(&unsigned_bytes, kp.as_ref())
                .map_err(|e| AppError::bad_request(format!("sign: {e}")))?;
            let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);
            let intent_side = match side {
                PumpFunSide::Buy => "buy",
                PumpFunSide::Sell => "sell",
            };
            let snap = serde_json::json!({
                "route": "pumpfun",
                "side": intent_side,
                "virtual_sol_reserves": curve.virtual_sol_reserves,
                "virtual_token_reserves": curve.virtual_token_reserves,
                "amount_in_lamports": route.amount_lamports,
                "slippage_bps": slippage_bps,
            });
            // Only buys create the token ATA — sells operate on
            // existing holdings (the token ATA must already exist for
            // the sell to be possible).
            let atas: Vec<Pubkey> = match side {
                PumpFunSide::Buy => vec![mint_pk],
                PumpFunSide::Sell => Vec::new(),
            };
            (signed_b64, intent_side, Some(snap), atas)
        }
        CachedRouteKind::PumpFunAmm {
            pool_pubkey,
            pool,
            reserves,
            base_token_program,
            quote_token_program,
            side,
        } => {
            let user = kp.pubkey();
            let blockhash = s
                .blockhash_cache
                .get()
                .await
                .map_err(|e| AppError::bad_gateway(format!("blockhash: {e}")))?;
            let unsigned_bytes = match side {
                PumpFunSide::Buy => {
                    let params = pumpfun_amm::BuyTxParams {
                        user,
                        pool: *pool_pubkey,
                        base_mint: pool.base_mint,
                        quote_mint: pool.quote_mint,
                        base_token_program: *base_token_program,
                        quote_token_program: *quote_token_program,
                        coin_creator: pool.coin_creator,
                        quote_in_amount: route.amount_lamports,
                        slippage_bps,
                        reserves: *reserves,
                        recent_blockhash: blockhash,
                        compute_unit_limit: 200_000,
                        compute_unit_price_micro_lamports: 50_000,
                        skip_base_ata_create: s.ata_cache.is_known(&user, &pool.base_mint),
                        skip_quote_ata_create: s.ata_cache.is_known(&user, &pool.quote_mint),
                    };
                    pumpfun_amm::build_buy_tx(&params)
                        .map_err(|e| AppError::bad_request(format!("build_buy_tx (amm): {e}")))?
                }
                PumpFunSide::Sell => {
                    let params = pumpfun_amm::SellTxParams {
                        user,
                        pool: *pool_pubkey,
                        base_mint: pool.base_mint,
                        quote_mint: pool.quote_mint,
                        base_token_program: *base_token_program,
                        quote_token_program: *quote_token_program,
                        coin_creator: pool.coin_creator,
                        base_in_amount: route.amount_lamports,
                        slippage_bps,
                        reserves: *reserves,
                        recent_blockhash: blockhash,
                        compute_unit_limit: 150_000,
                        compute_unit_price_micro_lamports: 50_000,
                        skip_quote_ata_create: s.ata_cache.is_known(&user, &pool.quote_mint),
                    };
                    pumpfun_amm::build_sell_tx(&params)
                        .map_err(|e| AppError::bad_request(format!("build_sell_tx (amm): {e}")))?
                }
            };
            let tx: solana_sdk::transaction::VersionedTransaction =
                bincode::deserialize(&unsigned_bytes)
                    .map_err(|e| AppError::bad_request(format!("decode tx: {e}")))?;
            allow
                .check_tx(&tx)
                .map_err(|e| AppError::bad_request(format!("program allowlist: {e}")))?;
            let signed_bytes = sign_versioned_tx_bytes(&unsigned_bytes, kp.as_ref())
                .map_err(|e| AppError::bad_request(format!("sign: {e}")))?;
            let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);
            let intent_side = match side {
                PumpFunSide::Buy => "buy",
                PumpFunSide::Sell => "sell",
            };
            let snap = serde_json::json!({
                "route": "pumpfun_amm",
                "side": intent_side,
                "pool": pool_pubkey.to_string(),
                "base_reserve": reserves.base,
                "quote_reserve": reserves.quote,
                "amount_in_lamports": route.amount_lamports,
                "slippage_bps": slippage_bps,
            });
            let atas: Vec<Pubkey> = match side {
                PumpFunSide::Buy => vec![pool.base_mint, pool.quote_mint],
                PumpFunSide::Sell => vec![pool.quote_mint],
            };
            (signed_b64, intent_side, Some(snap), atas)
        }
        CachedRouteKind::Raydium {
            amm_pubkey,
            amm,
            market,
            reserves,
            side,
        } => {
            let user = kp.pubkey();
            let blockhash = s
                .blockhash_cache
                .get()
                .await
                .map_err(|e| AppError::bad_gateway(format!("blockhash: {e}")))?;
            let unsigned_bytes = match side {
                PumpFunSide::Buy => {
                    let params = raydium_amm_v4::BuyTxParams {
                        user,
                        amm_pubkey: *amm_pubkey,
                        amm: *amm,
                        market: *market,
                        reserves: *reserves,
                        quote_in_lamports: route.amount_lamports,
                        slippage_bps,
                        recent_blockhash: blockhash,
                        compute_unit_limit: 220_000,
                        compute_unit_price_micro_lamports: 50_000,
                        skip_coin_ata_create: s.ata_cache.is_known(&user, &amm.coin_mint),
                        skip_pc_ata_create: s.ata_cache.is_known(&user, &amm.pc_mint),
                    };
                    raydium_amm_v4::build_buy_tx(&params).map_err(|e| {
                        AppError::bad_request(format!("build_buy_tx (raydium): {e}"))
                    })?
                }
                PumpFunSide::Sell => {
                    let params = raydium_amm_v4::SellTxParams {
                        user,
                        amm_pubkey: *amm_pubkey,
                        amm: *amm,
                        market: *market,
                        reserves: *reserves,
                        coin_in_amount: route.amount_lamports,
                        slippage_bps,
                        recent_blockhash: blockhash,
                        compute_unit_limit: 180_000,
                        compute_unit_price_micro_lamports: 50_000,
                        skip_pc_ata_create: s.ata_cache.is_known(&user, &amm.pc_mint),
                    };
                    raydium_amm_v4::build_sell_tx(&params).map_err(|e| {
                        AppError::bad_request(format!("build_sell_tx (raydium): {e}"))
                    })?
                }
            };
            let tx: solana_sdk::transaction::VersionedTransaction =
                bincode::deserialize(&unsigned_bytes)
                    .map_err(|e| AppError::bad_request(format!("decode tx (raydium): {e}")))?;
            allow
                .check_tx(&tx)
                .map_err(|e| AppError::bad_request(format!("program allowlist: {e}")))?;
            let signed_bytes = sign_versioned_tx_bytes(&unsigned_bytes, kp.as_ref())
                .map_err(|e| AppError::bad_request(format!("sign: {e}")))?;
            let signed_b64 = base64::engine::general_purpose::STANDARD.encode(&signed_bytes);
            let intent_side = match side {
                PumpFunSide::Buy => "buy",
                PumpFunSide::Sell => "sell",
            };
            let snap = serde_json::json!({
                "route": "raydium_amm_v4",
                "side": intent_side,
                "amm": amm_pubkey.to_string(),
                "coin_reserve": reserves.coin,
                "pc_reserve": reserves.pc,
                "amount_in_lamports": route.amount_lamports,
                "slippage_bps": slippage_bps,
            });
            let atas: Vec<Pubkey> = match side {
                PumpFunSide::Buy => vec![amm.coin_mint, amm.pc_mint],
                PumpFunSide::Sell => vec![amm.pc_mint],
            };
            (signed_b64, intent_side, Some(snap), atas)
        }
    };

    let relay = RelayClient::new(cfg.gateway_base, token);
    // Reuse the web-UI-pre-created intent when one was passed so we
    // don't orphan it in `pending`. Bot sessions / CLI callers still
    // hit the legacy create-then-submit path.
    let intent_id = match req.intent_id {
        Some(id) => id,
        None => {
            relay
                .create_intent(&CreateIntentReq {
                    side: intent_side.to_string(),
                    input_mint: route.input_mint,
                    output_mint: route.output_mint,
                    amount_in_lamports: route.amount_lamports as i64,
                    slippage_bps: Some(slippage_bps as i32),
                    submit_mode: Some("falcon_jito".into()),
                    tip_lamports: Some(tip_lamports),
                    preset_id: None,
                    bot_session_id: None,
                    copy_config_id: None,
                    quote_snapshot,
                    client_token: Some(format!("dbx:swap:{}", req.route_id)),
                })
                .await
                .map_err(|e| AppError::bad_gateway(format!("create_intent: {e}")))?
                .id
        }
    };
    let resp = relay
        .submit(&intent_id, &signed_b64, Some("falcon_jito"))
        .await
        .map_err(|e| AppError::bad_gateway(format!("submit: {e}")))?;

    let winner = resp
        .orders
        .iter()
        .find(|o| o.error.is_none())
        .ok_or_else(|| {
            let first_err = resp
                .orders
                .first()
                .and_then(|o| o.error.clone())
                .unwrap_or_else(|| "all paths failed".into());
            AppError::bad_gateway(format!("submit failed: {first_err}"))
        })?;

    // Submit succeeded — the ATA-create instructions we included will
    // have run, so the relevant ATAs definitely exist now. Record them
    // so future trades on this wallet skip the redundant create-IX.
    let user = kp.pubkey();
    for mint in atas_to_mark {
        s.ata_cache.mark_known(user, mint);
    }

    Ok(Json(SwapResult {
        tx_signature: winner.signature.clone(),
    }))
}

// ─── bot enable / disable ─────────────────────────────────────────

fn default_slippage_bps() -> u16 {
    100
}
fn default_submit_mode() -> String {
    "falcon_jito".into()
}
fn default_bot_tip_lamports() -> i64 {
    1_000_000
}
fn default_max_age_secs() -> i64 {
    120
}

/// Request body for `POST /bot/enable`.
///
/// The frontend creates the backend bot-session row via
/// `POST /api/trading/bot/sessions`, then passes the resulting
/// `session_id` here so the daemon can tag every intent it submits
/// with that id (enabling per-session spend tracking on the gateway).
#[derive(Debug, Deserialize)]
struct BotEnableBody {
    /// UUID of the backend `trading_bot_sessions` row — created by
    /// the frontend before calling /bot/enable.
    session_id: String,
    preset_id: String,
    /// SOL amount per trade in lamports.
    per_trade_lamports: u64,
    /// Hard session-level cap in lamports. Bot stops when this is
    /// reached.
    session_budget_lamports: u64,
    /// Optional per-token cap in lamports.
    #[serde(default)]
    per_token_lamports: Option<u64>,
    #[serde(default = "default_slippage_bps")]
    slippage_bps: u16,
    #[serde(default = "default_submit_mode")]
    submit_mode: String,
    #[serde(default = "default_bot_tip_lamports")]
    tip_lamports: i64,
    #[serde(default)]
    min_mcap_usd: Option<f64>,
    #[serde(default)]
    min_liquidity_usd: Option<f64>,
    /// Signals older than this are skipped (prevents re-firing stale
    /// matches after a reconnect).
    #[serde(default = "default_max_age_secs")]
    max_age_secs: i64,
    #[serde(default)]
    skip_simulate: bool,
    #[serde(default)]
    skip_allowlist: bool,
}

#[derive(Debug, Serialize)]
struct BotEnableResult {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "presetId")]
    preset_id: String,
}

#[derive(Debug, Deserialize)]
struct BotDisableBody {
    /// When present the daemon verifies it matches the active session
    /// before aborting; omit to stop whatever is running.
    #[serde(default)]
    session_id: Option<String>,
}

async fn bot_enable(
    State(s): State<DaemonState>,
    Json(body): Json<BotEnableBody>,
) -> Result<Json<BotEnableResult>, AppError> {
    let cfg = s.config.read().await.clone();
    let token = cfg
        .auth_token
        .clone()
        .ok_or_else(|| AppError::forbidden("no auth token — call /setAuth first"))?;
    let kp = s.kp.unlocked().ok_or_else(AppError::locked)?;

    if body.per_trade_lamports == 0 {
        return Err(AppError::bad_request("per_trade_lamports must be > 0"));
    }
    if body.session_budget_lamports < body.per_trade_lamports {
        return Err(AppError::bad_request(
            "session_budget_lamports must be >= per_trade_lamports",
        ));
    }

    let matcher = PresetMatcher {
        min_mcap_usd: body.min_mcap_usd,
        max_mcap_usd: None,
        min_liquidity_usd: body.min_liquidity_usd,
        max_age_secs: Some(body.max_age_secs),
        blocked_tokens: Default::default(),
    };
    let budget = BudgetState::new(BudgetConfig {
        session_budget_lamports: body.session_budget_lamports,
        per_token_cap_lamports: body.per_token_lamports,
        per_hour_cap_lamports: None,
    });
    let allowlist =
        default_allowlist().map_err(|e| AppError::internal(format!("allowlist: {e}")))?;
    let bot_cfg = BotConfig {
        per_trade_lamports: body.per_trade_lamports,
        slippage_bps: body.slippage_bps,
        tip_lamports: body.tip_lamports,
        submit_mode: body.submit_mode.clone(),
        rpc_url: s.rpc_url.clone(),
        skip_simulate: body.skip_simulate,
        skip_allowlist: body.skip_allowlist,
        input_mint: "So11111111111111111111111111111111111111112".into(),
        pumpfun_cu_limit: 120_000,
        pumpfun_cu_price_micro_lamports: 50_000,
        bot_session_id: Some(body.session_id.clone()),
        preset_id: Some(body.preset_id.clone()),
    };
    // Default bot trades at the Turbo tier — auto-buys prioritise
    // landing over fee economy, but stay below the Max tier which is
    // reserved for explicit snipe sessions.
    let mut engine = BotEngine::new(matcher, budget, allowlist, bot_cfg)
        .with_blockhash_cache(s.blockhash_cache.clone())
        .with_ata_cache(s.ata_cache.clone())
        .with_fee_strategy(s.fee_strategy.clone(), FeeTier::Turbo);

    // Subscribe to the gateway WS signal stream for this preset.
    // spawn_subscriber opens the WS connection and starts a reconnect
    // loop; we receive Signals via the returned mpsc channel.
    let mut rx = spawn_subscriber(
        cfg.gateway_base.clone(),
        token.clone(),
        vec![body.preset_id.clone()],
    )
    .await
    .map_err(|e| AppError::bad_gateway(format!("ws subscribe: {e}")))?;

    // Clone all long-lived refs the task needs; the Arc clones are cheap.
    // (`kp` was pinned above — the session keeps signing with the key it
    // was armed with even if the host relocks mid-session; disable to stop.)
    let jup = s.jup.clone();
    let rpc = s.rpc.clone();
    let relay = RelayClient::new(cfg.gateway_base.clone(), token);
    let session_id_log = body.session_id.clone();
    let preset_id_log = body.preset_id.clone();

    let handle = tokio::spawn(async move {
        while let Some(sig) = rx.recv().await {
            let addr = sig.token_address.clone();
            match engine.handle_one(sig, &jup, &relay, &rpc, &kp).await {
                Ok(Decision::Submitted(resp)) => {
                    let sig_str = resp
                        .orders
                        .first()
                        .map(|o| {
                            let end = o.signature.len().min(16);
                            o.signature[..end].to_string()
                        })
                        .unwrap_or_else(|| "no orders".into());
                    tracing::info!(
                        session = %session_id_log,
                        preset = %preset_id_log,
                        token = %addr,
                        signature = %sig_str,
                        "bot: trade submitted",
                    );
                }
                Ok(Decision::Skipped(reason)) => {
                    tracing::debug!(
                        session = %session_id_log,
                        token = %addr,
                        %reason,
                        "bot: signal skipped",
                    );
                }
                Err(e) => {
                    let msg = format!("{e}");
                    if msg.contains("session budget exhausted") {
                        tracing::info!(
                            session = %session_id_log,
                            "bot: session budget exhausted — task ending",
                        );
                        break;
                    }
                    tracing::warn!(
                        session = %session_id_log,
                        token = %addr,
                        ?e,
                        "bot: signal error",
                    );
                }
            }
        }
        tracing::info!(session = %session_id_log, "bot: task ended");
    });

    // Arm — replace any prior session atomically.
    let mut active = s.active_bot.lock().await;
    if let Some(prev) = active.take() {
        prev.handle.abort();
        tracing::info!(
            prev_session = %prev.session_id,
            "bot: prior session aborted by new /bot/enable",
        );
    }
    *active = Some(ActiveBot {
        session_id: body.session_id.clone(),
        preset_id: body.preset_id.clone(),
        handle,
    });

    tracing::info!(
        session = %body.session_id,
        preset = %body.preset_id,
        per_trade_lamports = body.per_trade_lamports,
        session_budget_lamports = body.session_budget_lamports,
        "bot: session armed",
    );
    Ok(Json(BotEnableResult {
        session_id: body.session_id,
        preset_id: body.preset_id,
    }))
}

async fn bot_disable(
    State(s): State<DaemonState>,
    Json(body): Json<BotDisableBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    // Grab and clear the active bot under the lock. Don't hold the
    // Mutex across the async relay call below.
    let bot = {
        let mut guard = s.active_bot.lock().await;
        match guard.take() {
            None => return Err(AppError::bad_request("no active bot session")),
            Some(b) => {
                if let Some(ref id) = body.session_id {
                    if b.session_id != *id {
                        let actual = b.session_id.clone();
                        // Put it back and reject — caller sent the wrong id.
                        *guard = Some(b);
                        return Err(AppError::bad_request(format!(
                            "session_id mismatch: active session is {actual}",
                        )));
                    }
                }
                b
            }
        }
    };

    bot.handle.abort();
    tracing::info!(session = %bot.session_id, "bot: session disabled via /bot/disable");

    // Best-effort: mark the gateway session cancelled so the dashboard
    // shows the right status without waiting for the expiry sweep.
    // Errors here don't affect the 204 — the local loop IS stopped.
    let cfg = s.config.read().await.clone();
    if let Some(token) = cfg.auth_token {
        let relay = RelayClient::new(cfg.gateway_base, token);
        if let Err(e) = relay.cancel_bot_session(&bot.session_id).await {
            tracing::warn!(
                ?e,
                session = %bot.session_id,
                "bot: failed to cancel session on gateway (best-effort)",
            );
        }
    }

    Ok(Json(serde_json::Value::Null))
}

#[derive(Debug, Deserialize)]
struct SetAuthBody {
    token: Option<String>,
}

// DIVERGENCE from the signer-cli daemon (deliberate): these mutation
// endpoints answer `200` with a JSON `null` body instead of `204`. The
// web app's daemon client unconditionally `res.json()`s the response —
// a 204's empty body makes that throw, `pushSignerAuth` swallows the
// throw, and the follow-up `setAuth` never runs, so every `/swap`
// 403s with "no auth token". Returning a parseable body keeps the
// frozen contract (`Promise<void>` — callers ignore the value) while
// actually working from the browser.
async fn set_auth(
    State(s): State<DaemonState>,
    Json(body): Json<SetAuthBody>,
) -> Json<serde_json::Value> {
    s.config.write().await.auth_token = body.token.filter(|t| !t.is_empty());
    Json(serde_json::Value::Null)
}

#[derive(Debug, Deserialize)]
struct SetGatewayBody {
    base: String,
}

async fn set_gateway(
    State(s): State<DaemonState>,
    Json(body): Json<SetGatewayBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if !(body.base.starts_with("http://") || body.base.starts_with("https://")) {
        return Err(AppError::bad_request(
            "base must start with http:// or https://",
        ));
    }
    s.config.write().await.gateway_base = body.base;
    Ok(Json(serde_json::Value::Null))
}

// ─── small helpers ───────────────────────────────────────────────────

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    fn bad_request(m: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: m.into(),
        }
    }
    fn forbidden(m: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: m.into(),
        }
    }
    /// 409 — the request is well-formed but stale against live state
    /// (e.g. the min-out floor can no longer be honoured). The caller
    /// should re-quote and retry.
    fn conflict(m: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: m.into(),
        }
    }
    /// 503 — keystore locked; the host app must unlock first. The web
    /// app surfaces this verbatim so the user knows to open the app.
    fn locked() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "signer locked — open the DegenBox Signer app and unlock".into(),
        }
    }
    fn bad_gateway(m: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: m.into(),
        }
    }
    fn internal(m: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: m.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

/// Tiny UUID-v4-shaped string for route IDs. No external dep — the
/// daemon doesn't otherwise need `uuid`, and we get random IDs for
/// free from the OS RNG.
fn uuid_v4_string() -> String {
    use rand_core::RngCore;
    let mut buf = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut buf);
    // RFC 4122 v4 stamping.
    buf[6] = (buf[6] & 0x0f) | 0x40;
    buf[8] = (buf[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        buf[0], buf[1], buf[2], buf[3],
        buf[4], buf[5],
        buf[6], buf[7],
        buf[8], buf[9],
        buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_port_is_5829() {
        // Stable so the web app can hard-code the probe URL.
        assert_eq!(default_port(), 5829);
    }

    // ── SignerSlot lock/unlock semantics (the host-app divergence
    //    from the CLI origin, where the key was pinned at boot) ──

    #[test]
    fn signer_slot_install_unlock_clear() {
        let slot = SignerSlot::default();
        assert!(slot.unlocked().is_none(), "fresh slot starts locked");
        let kp = Keypair::new();
        let pubkey = kp.pubkey();
        slot.install(kp);
        let got = slot.unlocked().expect("unlocked after install");
        assert_eq!(got.pubkey(), pubkey);
        // Clones observe the same slot (the daemon holds a clone).
        let clone = slot.clone();
        assert!(clone.unlocked().is_some());
        slot.clear();
        assert!(clone.unlocked().is_none(), "clear relocks every clone");
        // An Arc handed out before the clear keeps signing its in-flight
        // request — pinned, not dangling.
        assert_eq!(got.pubkey(), pubkey);
    }

    #[tokio::test]
    async fn connect_and_status_respect_the_lock() {
        let slot = SignerSlot::default();
        let state = DaemonState::new(
            slot.clone(),
            "https://gw.example".into(),
            "https://rpc.example".into(),
        );

        // Locked: /connect refuses with 503, /status says disconnected.
        let err = connect(State(state.clone())).await.expect_err("locked");
        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        let st = status(State(state.clone())).await.0;
        assert!(!st.connected);
        assert!(st.pubkey.is_none());
        assert_eq!(st.client_kind, "signer-app");
        assert_eq!(st.version, crate::VERSION);

        // Unlock: both flip live with the installed key.
        let kp = Keypair::new();
        let pubkey = kp.pubkey().to_string();
        slot.install(kp);
        let ok = connect(State(state.clone())).await.expect("unlocked").0;
        assert_eq!(ok.pubkey, pubkey);
        assert_eq!(ok.balance_lamports, 0);
        let st = status(State(state.clone())).await.0;
        assert!(st.connected);
        assert_eq!(st.pubkey.as_deref(), Some(pubkey.as_str()));

        // Relock: refused again.
        slot.clear();
        assert!(connect(State(state)).await.is_err());
    }

    #[test]
    fn uuid_v4_string_has_correct_shape() {
        let s = uuid_v4_string();
        assert_eq!(s.len(), 36, "8-4-4-4-12 with 4 dashes");
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        // Version nibble.
        assert!(s.chars().nth(14).unwrap() == '4');
        // RFC 4122 variant nibble: 8, 9, a, or b.
        assert!(matches!(s.chars().nth(19).unwrap(), '8' | '9' | 'a' | 'b'));
    }

    #[test]
    fn uuid_v4_string_is_random() {
        // Belt-and-braces — two consecutive calls should not collide.
        let a = uuid_v4_string();
        let b = uuid_v4_string();
        assert_ne!(a, b);
    }

    fn dummy_quote() -> QuoteResponse {
        QuoteResponse {
            input_mint: "x".into(),
            output_mint: "y".into(),
            in_amount: "1".into(),
            out_amount: "1".into(),
            other_amount_threshold: "1".into(),
            swap_mode: "ExactIn".into(),
            slippage_bps: 0,
            price_impact_pct: "0".into(),
            extra: Default::default(),
        }
    }

    fn dummy_curve() -> BondingCurveAccount {
        BondingCurveAccount {
            virtual_token_reserves: pumpfun::INITIAL_VIRTUAL_TOKEN_RESERVES,
            virtual_sol_reserves: pumpfun::INITIAL_VIRTUAL_SOL_RESERVES,
            real_token_reserves: pumpfun::INITIAL_REAL_TOKEN_RESERVES,
            real_sol_reserves: 0,
            token_total_supply: 1_000_000_000_000_000,
            complete: false,
            creator: solana_sdk::pubkey::Pubkey::new_unique(),
        }
    }

    #[test]
    fn route_cache_consumes_and_expires() {
        let mut cache = RouteCache::default();
        let route = CachedRoute {
            kind: CachedRouteKind::Jupiter(dummy_quote()),
            input_mint: "x".into(),
            output_mint: "y".into(),
            amount_lamports: 1_000_000,
            expected_out_lamports: 900,
            min_out_lamports: 800,
            slippage_bps: 100,
            expires_at_unix_ms: now_ms() + 30_000,
        };
        cache.insert("id".into(), route.clone());
        // First consume: hit.
        let r = cache.consume("id").expect("present");
        assert_eq!(r.amount_lamports, route.amount_lamports);
        // Second consume: miss (already removed).
        assert!(cache.consume("id").is_none());

        // Pre-expired route → consume returns None.
        let expired = CachedRoute {
            expires_at_unix_ms: now_ms().saturating_sub(1),
            ..route
        };
        cache.insert("stale".into(), expired);
        assert!(cache.consume("stale").is_none());
    }

    #[test]
    fn route_cache_evicts_at_capacity() {
        let mut cache = RouteCache::default();
        // Fill past the threshold; the janitor must keep the size bounded
        // by pruning expired entries on insert (we make most expired).
        for i in 0..40 {
            let r = CachedRoute {
                kind: CachedRouteKind::Jupiter(dummy_quote()),
                input_mint: "x".into(),
                output_mint: "y".into(),
                amount_lamports: 1,
                expected_out_lamports: 1,
                min_out_lamports: 1,
                slippage_bps: 0,
                // Half expired, half fresh.
                expires_at_unix_ms: if i % 2 == 0 {
                    now_ms().saturating_sub(1)
                } else {
                    now_ms() + 30_000
                },
            };
            cache.insert(format!("k{i}"), r);
        }
        // After the run, fresh entries should still be retrievable.
        let alive = (0..40).filter(|i| i % 2 == 1).count();
        let still = (0..40)
            .filter(|i| i % 2 == 1)
            .filter(|i| cache.consume(&format!("k{i}")).is_some())
            .count();
        assert_eq!(still, alive, "all unexpired entries must remain");
    }

    // ── M17: the minOutLamports floor ─────────────────────────────

    #[test]
    fn jupiter_route_effective_min_out_is_the_cached_quote_floor() {
        // Jupiter swaps replay the cached quote verbatim — a slippage
        // override on /swap does NOT re-quote, so the enforced floor is
        // exactly what the quote promised.
        let kind = CachedRouteKind::Jupiter(dummy_quote());
        assert_eq!(
            route_effective_min_out(&kind, 800, 1_000_000, 50),
            Some(800)
        );
        assert_eq!(
            route_effective_min_out(&kind, 800, 1_000_000, 5_000),
            Some(800),
            "slippage override must not change a replayed Jupiter quote"
        );
    }

    #[test]
    fn native_route_min_out_recomputes_with_effective_slippage() {
        let curve = dummy_curve();
        let kind = CachedRouteKind::PumpFun {
            curve,
            token_program: solana_sdk::pubkey::Pubkey::new_unique(),
            side: PumpFunSide::Buy,
        };
        let amount = 1_000_000_000u64;
        let tight = route_effective_min_out(&kind, 0, amount, 50).unwrap();
        let loose = route_effective_min_out(&kind, 0, amount, 2_000).unwrap();
        assert!(
            loose < tight,
            "a looser swap-time slippage weakens the enforced floor — \
             exactly the case the 409 must catch ({loose} !< {tight})"
        );
        // The floor check itself: a floor taken from the tight quote
        // trips when the effective floor is the loose one.
        let floor = tight;
        assert!(loose < floor, "409 path: effective < floor");
        assert!(tight >= floor, "happy path: effective >= floor");
    }

    #[test]
    fn native_route_min_out_overflow_is_none() {
        let kind = CachedRouteKind::PumpFunAmm {
            pool_pubkey: solana_sdk::pubkey::Pubkey::new_unique(),
            pool: PoolAccount {
                pool_bump: 0,
                index: 0,
                creator: solana_sdk::pubkey::Pubkey::new_unique(),
                base_mint: solana_sdk::pubkey::Pubkey::new_unique(),
                quote_mint: solana_sdk::pubkey::Pubkey::new_unique(),
                lp_mint: solana_sdk::pubkey::Pubkey::new_unique(),
                pool_base_token_account: solana_sdk::pubkey::Pubkey::new_unique(),
                pool_quote_token_account: solana_sdk::pubkey::Pubkey::new_unique(),
                lp_supply: 0,
                coin_creator: solana_sdk::pubkey::Pubkey::new_unique(),
            },
            reserves: PoolReserves { base: 0, quote: 1 },
            base_token_program: solana_sdk::pubkey::Pubkey::new_unique(),
            quote_token_program: solana_sdk::pubkey::Pubkey::new_unique(),
            side: PumpFunSide::Buy,
        };
        assert_eq!(route_effective_min_out(&kind, 1, 1_000, 100), None);
    }

    // ── PumpFun quote-amounts dispatch ────────────────────────────

    #[test]
    fn pumpfun_quote_buy_inflates_expected_with_curve_math() {
        let curve = dummy_curve();
        // 1 SOL on fresh curve → ~34.6M tokens (sanity from
        // pumpfun::buy_quote tests).
        let (expected, min_out) =
            pumpfun_quote_amounts(&curve, &PumpFunSide::Buy, 1_000_000_000, 100).unwrap();
        assert!(expected > 34_000_000_000_000 && expected < 35_000_000_000_000);
        // 1% slippage on the buy = min_out ~99% of expected.
        assert!(min_out < expected);
        // Ratio in the right ballpark (deflated, not zeroed).
        let ratio = (min_out as f64) / (expected as f64);
        assert!(ratio > 0.98 && ratio < 1.0, "ratio={ratio}");
    }

    #[test]
    fn pumpfun_quote_sell_returns_sol_against_curve() {
        let curve = dummy_curve();
        // Sell some tokens → some SOL out. Drift test: amount should
        // be small but non-zero against fresh-curve reserves.
        let (expected, min_out) =
            pumpfun_quote_amounts(&curve, &PumpFunSide::Sell, 100_000_000, 50).unwrap();
        assert!(expected > 0);
        assert!(min_out > 0);
        assert!(min_out <= expected);
    }

    #[test]
    fn pumpfun_quote_zero_slippage_means_no_deflation() {
        let curve = dummy_curve();
        let (expected, min_out) =
            pumpfun_quote_amounts(&curve, &PumpFunSide::Buy, 500_000_000, 0).unwrap();
        assert_eq!(expected, min_out);
    }

    #[test]
    fn pumpfun_quote_amount_zero_is_handled() {
        // Buy with 0 lamports → 0 tokens. Not None — the curve math
        // returns Some(0).
        let curve = dummy_curve();
        let r = pumpfun_quote_amounts(&curve, &PumpFunSide::Buy, 0, 100).unwrap();
        assert_eq!(r, (0, 0));
    }

    // ── PumpFun-AMM quote-amounts dispatch ────────────────────────

    #[test]
    fn pumpfun_amm_quote_buy_against_pool_reserves() {
        // Modest pool: 1B base tokens × 100 SOL quote.
        let r = PoolReserves {
            base: 1_000_000_000,
            quote: 100_000_000_000,
        };
        let (expected, min_out) =
            pumpfun_amm_quote_amounts(&r, &PumpFunSide::Buy, 1_000_000_000, 100).unwrap();
        // Should land near pumpfun_amm::buy_quote(1 SOL, 1B, 100 SOL).
        assert!(expected > 0);
        // 1% slippage → min_out ~99% of expected.
        let ratio = (min_out as f64) / (expected as f64);
        assert!(ratio > 0.98 && ratio < 1.0, "ratio={ratio}");
    }

    #[test]
    fn pumpfun_amm_quote_sell_returns_quote_token() {
        let r = PoolReserves {
            base: 1_000_000_000,
            quote: 100_000_000_000,
        };
        let (expected, min_out) =
            pumpfun_amm_quote_amounts(&r, &PumpFunSide::Sell, 10_000_000, 50).unwrap();
        // Sell 10M base into a 1B pool — should yield ~1 SOL out.
        assert!(expected > 0);
        assert!(min_out > 0);
        assert!(min_out <= expected);
    }

    #[test]
    fn pumpfun_amm_quote_zero_reserves_is_none() {
        let r = PoolReserves { base: 0, quote: 1 };
        assert!(pumpfun_amm_quote_amounts(&r, &PumpFunSide::Buy, 1_000, 100).is_none());
    }

    // ── Raydium AMM v4 quote-amounts dispatch ─────────────────────

    fn dummy_raydium_amm() -> RaydiumAmm {
        use solana_sdk::pubkey;
        RaydiumAmm {
            status: 1,
            swap_fee_numerator: 25,
            swap_fee_denominator: 10_000,
            token_coin: pubkey!("11111111111111111111111111111111"),
            token_pc: pubkey!("So11111111111111111111111111111111111111112"),
            coin_mint: pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            pc_mint: pubkey!("So11111111111111111111111111111111111111112"),
            open_orders: pubkey!("11111111111111111111111111111111"),
            market: pubkey!("11111111111111111111111111111111"),
            serum_dex: pubkey!("11111111111111111111111111111111"),
            target_orders: pubkey!("11111111111111111111111111111111"),
        }
    }

    fn dummy_raydium_reserves() -> RaydiumReserves {
        // Pool: 1B coin tokens × 100 SOL (1e11 lamports) pc.
        RaydiumReserves {
            coin: 1_000_000_000,
            pc: 100_000_000_000,
        }
    }

    #[test]
    fn raydium_quote_buy_applies_fee_and_slippage() {
        let amm = dummy_raydium_amm();
        let reserves = dummy_raydium_reserves();
        // Buy 1 SOL worth of tokens.
        let (expected, min_out) =
            raydium_quote_amounts(&amm, &reserves, &PumpFunSide::Buy, 1_000_000_000, 100).unwrap();
        // 0.25% fee on input → slightly less coin out than naive CP.
        assert!(expected > 0);
        // 1% slippage deflates min_out.
        let ratio = min_out as f64 / expected as f64;
        assert!(ratio > 0.98 && ratio < 1.0, "buy slippage ratio={ratio}");
    }

    #[test]
    fn raydium_quote_sell_applies_fee_and_slippage() {
        let amm = dummy_raydium_amm();
        let reserves = dummy_raydium_reserves();
        // Sell 10M coin tokens.
        let (expected, min_out) =
            raydium_quote_amounts(&amm, &reserves, &PumpFunSide::Sell, 10_000_000, 50).unwrap();
        assert!(expected > 0);
        assert!(min_out <= expected);
        // 0.5% slippage.
        let ratio = min_out as f64 / expected as f64;
        assert!(ratio > 0.99 && ratio <= 1.0, "sell slippage ratio={ratio}");
    }

    #[test]
    fn raydium_quote_zero_reserves_returns_none() {
        let amm = dummy_raydium_amm();
        let empty = RaydiumReserves {
            coin: 0,
            pc: 1_000_000_000,
        };
        assert!(raydium_quote_amounts(&amm, &empty, &PumpFunSide::Buy, 1_000_000, 100).is_none());
    }

    #[test]
    fn raydium_quote_zero_slippage_gives_equal_expected_and_min() {
        let amm = dummy_raydium_amm();
        let reserves = dummy_raydium_reserves();
        let (expected, min_out) =
            raydium_quote_amounts(&amm, &reserves, &PumpFunSide::Buy, 500_000_000, 0).unwrap();
        assert_eq!(expected, min_out);
    }
}
