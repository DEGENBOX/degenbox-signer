//! Read-side gateway client for the Solana GUI surfaces.
//!
//! Auth resolution order (the app's "existing gateway auth"):
//!
//! 1. The persisted pairing JWT in the shared HL config
//!    (`hl-config.json` → `api_token`) — minted by the gateway's
//!    `redeem-registration` flow as a normal `AuthClaims` user JWT, so
//!    it works on `/auth/me` and every `/api/trading/*` route, not
//!    just `/signer/*`.
//! 2. The session token the DegenBox web app pushes to the `:5829`
//!    signer-protocol daemon via `POST /setAuth` (in-memory only).
//!    Covers Solana-only users who never pair an HL agent.
//!
//! Everything here is read-only against the gateway except
//! `set_copy_config_enabled` (PATCH on the caller's own config — the
//! server engine remains the policy layer).

use crate::hl::config::HlConfig;
use crate::state::AppState;
use chrono::{DateTime, Duration, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct GatewayAuth {
    pub base: String,
    pub token: String,
}

/// Resolve gateway credentials, or a user-actionable error.
///
/// Order:
/// 1. The Discord desktop login's JWT (`desktop-auth.json`) — the
///    primary auth path; one browser hand-off covers BOTH runtimes.
/// 2. The persisted HL pairing JWT (`hl-config.json` → `api_token`).
/// 3. The session token the DegenBox web app pushed to the `:5829`
///    daemon via `POST /setAuth` (in-memory only).
pub async fn resolve_auth(state: &AppState) -> Result<GatewayAuth, String> {
    // Every rung filters client-side-expired JWTs: handing the gateway
    // a token we KNOW is dead turns "please re-login" into a 401 storm
    // downstream (and used to trip the access-loss lock — expired is a
    // credential's normal lifecycle, not a revocation).
    let mut found_expired = false;
    if let Some(auth) = crate::auth::DesktopAuth::load_valid() {
        return Ok(GatewayAuth {
            base: auth.gateway_base.trim_end_matches('/').to_string(),
            token: auth.token,
        });
    } else if crate::auth::DesktopAuth::load().is_some() {
        found_expired = true;
    }
    let cfg = HlConfig::load_or_default();
    if let Some(token) = cfg.api_token.clone() {
        if jwt_expired(&token) {
            found_expired = true;
        } else {
            return Ok(GatewayAuth {
                base: cfg.server_url.trim_end_matches('/').to_string(),
                token,
            });
        }
    }
    // Fallback: token the web app handed the :5829 daemon this session.
    let web = state.web_auth.lock().ok().and_then(|g| g.as_ref().cloned());
    if let Some(rc) = web {
        let g = rc.read().await;
        if let Some(token) = g.auth_token.clone() {
            if jwt_expired(&token) {
                found_expired = true;
            } else {
                return Ok(GatewayAuth {
                    base: g.gateway_base.trim_end_matches('/').to_string(),
                    token,
                });
            }
        }
    }
    if found_expired {
        return Err(
            "gateway session expired — re-link your Discord account (account menu, top right) to \
             sign back in"
                .into(),
        );
    }
    Err(
        "not connected to DegenBox — link your Discord account (account menu, top right), pair this \
         signer, or open the DegenBox web app once so it can hand this client a session \
         token"
            .into(),
    )
}

/// True when the bearer is a JWT whose `exp` claim is already in the
/// past. Tokens that don't parse as a JWT (or carry no `exp`) pass as
/// usable — the gateway remains the verifier; this is purely a "don't
/// send what we know is dead" filter.
pub(crate) fn jwt_expired(token: &str) -> bool {
    use base64::Engine as _;
    let Some(payload_b64) = token.split('.').nth(1) else {
        return false;
    };
    let Ok(raw) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64.as_bytes())
    else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw) else {
        return false;
    };
    let Some(exp) = v.get("exp").and_then(|e| e.as_i64()) else {
        return false;
    };
    chrono::DateTime::from_timestamp(exp, 0).is_some_and(|t| t < Utc::now())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("reqwest client")
}

// ─── desktop JWT refresh (audit H1) ────────────────────────────────
//
// The Discord desktop-login JWT is a normal ~24 h session token. A
// signer that runs longer than that used to go silently dead: nothing
// in the app ever renewed the token. The gateway already exposes a
// legitimate programmatic renewal — `GET /auth/signer-token` mints a
// fresh JWT (same identity + roles, fresh jti/exp) for any
// still-valid bearer. We call it proactively from the Sol runtime's
// maintenance tick while the persisted token is alive, so a
// continuously-running signer never crosses the expiry cliff. An
// already-expired token cannot be refreshed (by design — renewal must
// not be an escalation path); that state surfaces as `auth_expired`
// in the runtime status and requires a re-login.

/// Refresh when less than this much lifetime remains. Generous: the
/// token lives 24 h, so any app that is awake for a few minutes in any
/// 12 h window stays fresh forever.
const REFRESH_WINDOW_SECS: i64 = 12 * 3600;

/// Decode the `exp` claim from a JWT without verifying the signature
/// (client-side bookkeeping only — the gateway remains the verifier).
fn jwt_exp_rfc3339(token: &str) -> Option<String> {
    use base64::Engine as _;
    let payload_b64 = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64.as_bytes())
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let exp = v.get("exp")?.as_i64()?;
    Some(chrono::DateTime::from_timestamp(exp, 0)?.to_rfc3339())
}

/// True when the persisted desktop token should be renewed now:
/// within [`REFRESH_WINDOW_SECS`] of expiry, or expiry unknown.
fn needs_refresh(expires_at: Option<&str>, now: DateTime<Utc>) -> bool {
    match expires_at.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) {
        Some(t) => (t.with_timezone(&Utc) - now).num_seconds() < REFRESH_WINDOW_SECS,
        // Unknown expiry → refresh opportunistically so we at least
        // learn the real `exp` from the freshly minted token.
        None => true,
    }
}

/// Proactively renew the persisted Discord desktop-login JWT via
/// `GET /auth/signer-token` when it is inside the refresh window.
///
/// Returns `Ok(Some(new_token))` when a refresh happened (the caller
/// should push it into the `:5829` daemon via
/// `auth::install_runtime_token`), `Ok(None)` when nothing needed
/// doing (no persisted login, already expired and hence
/// unrefreshable, or comfortably fresh), `Err` on a refresh attempt
/// that failed (network / gateway error) — the caller logs and the
/// next tick retries while the old token is still alive.
pub async fn refresh_desktop_auth_if_needed() -> Result<Option<String>, String> {
    // `load_valid` filters expired tokens: an expired credential can't
    // authenticate the refresh call anyway — that's the re-login case.
    let Some(auth) = crate::auth::DesktopAuth::load_valid() else {
        return Ok(None);
    };
    if !needs_refresh(auth.expires_at.as_deref(), Utc::now()) {
        return Ok(None);
    }
    let base = auth.gateway_base.trim_end_matches('/');
    let url = format!("{base}/auth/signer-token");
    let resp = http()
        .get(&url)
        .bearer_auth(&auth.token)
        .send()
        .await
        .map_err(|e| format!("GET /auth/signer-token: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GET /auth/signer-token: gateway {status}: {body}"));
    }
    #[derive(Deserialize)]
    struct TokenResp {
        token: String,
    }
    let parsed: TokenResp = resp
        .json()
        .await
        .map_err(|e| format!("GET /auth/signer-token: decode: {e}"))?;
    let refreshed = crate::auth::DesktopAuth {
        expires_at: jwt_exp_rfc3339(&parsed.token),
        token: parsed.token.clone(),
        discord: auth.discord,
        gateway_base: auth.gateway_base,
    };
    refreshed.save()?;
    tracing::info!(
        expires_at = ?refreshed.expires_at,
        "desktop gateway JWT refreshed via /auth/signer-token"
    );
    Ok(Some(parsed.token))
}

async fn get_json<T: DeserializeOwned>(auth: &GatewayAuth, path: &str) -> Result<T, String> {
    let url = format!("{}{}", auth.base, path);
    let resp = http()
        .get(&url)
        .bearer_auth(&auth.token)
        .send()
        .await
        .map_err(|e| format!("GET {path}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GET {path}: gateway {status}: {body}"));
    }
    resp.json::<T>()
        .await
        .map_err(|e| format!("GET {path}: decode: {e}"))
}

/// Generic authed JSON request for GUI surfaces whose endpoints have
/// no bespoke DTO mapping in this file (PnL windows, copy-trade
/// summary, preset detail, …). The gateway stays the policy layer —
/// this only forwards the caller's own bearer to `/api/*` paths.
pub async fn request_json_raw(
    auth: &GatewayAuth,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let url = format!("{}{}", auth.base, path);
    let client = http();
    let mut req = match method {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PATCH" => client.patch(&url),
        "PUT" => client.put(&url),
        "DELETE" => client.delete(&url),
        other => return Err(format!("unsupported method {other}")),
    };
    req = req.bearer_auth(&auth.token);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("{method} {path}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("{method} {path}: gateway {status}: {text}"));
    }
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("{method} {path}: decode: {e}"))
}

/// Tolerant numeric reader — the gateway serialises Decimals as JSON
/// strings and integers as numbers; the GUI only needs display floats.
fn num(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn fmt_amount(x: f64) -> String {
    if !x.is_finite() {
        return "—".into();
    }
    if x >= 1000.0 {
        format!("{x:.0}")
    } else if x >= 1.0 {
        format!("{x:.3}")
    } else {
        format!("{x:.6}")
    }
}

fn fmt_usd2(x: f64) -> String {
    format!("{x:.2}")
}

// ─── positions ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GwPosition {
    mint: String,
    #[serde(default)]
    symbol: Option<String>,
    net_amount: serde_json::Value,
    total_in_lamports: i64,
    #[serde(default)]
    total_out_lamports: i64,
    #[serde(default)]
    current_price_usd: Option<serde_json::Value>,
    #[serde(default)]
    sol_price_usd: Option<serde_json::Value>,
    #[serde(default)]
    first_fill_at: Option<String>,
    #[serde(default)]
    decimals: Option<i32>,
    // W3.1 — fields the gateway already returns that the DTO used to
    // drop (joined from `alpha_tokens` / the position roll-up).
    #[serde(default)]
    token_name: Option<String>,
    #[serde(default)]
    image_url: Option<String>,
    #[serde(default)]
    current_market_cap_usd: Option<serde_json::Value>,
    #[serde(default)]
    realized_pnl_lamports: i64,
    #[serde(default)]
    fill_count: i32,
}

/// GUI shape — must match `SolPosition` in `src/ipc.ts`.
#[derive(Debug, Serialize)]
pub struct SolPositionDto {
    pub mint: String,
    pub symbol: String,
    pub amount_ui: String,
    pub cost_usd: Option<String>,
    pub value_usd: Option<String>,
    pub pnl_usd: Option<String>,
    /// Per-position attribution (manual/bot/copytrade) is not carried
    /// on the gateway's position rows today — always `None`, rendered
    /// as "—". (Fills carry `source`; the aggregate doesn't.)
    pub source: Option<String>,
    pub opened_at: Option<String>,
    /// Live token price (USD, decimal string) — entry-price autofill
    /// for the TP/SL arm dialog. `None` when the mint isn't priced yet.
    pub current_price_usd: Option<String>,
    // ── W3.1 additions (Positions tab) ────────────────────────────
    // All decimals are plain numeric strings (NOT display-formatted);
    // the TS layer formats via @degenbox/ui formatters. Additive — the
    // legacy `SolPosition` consumers (Home/ClientDetail) ignore them.
    /// Token display name from `alpha_tokens`.
    pub name: Option<String>,
    /// Token logo URL from `alpha_tokens`.
    pub image_url: Option<String>,
    /// Live market cap (USD).
    pub mcap_usd: Option<String>,
    /// Market cap at the average entry price — `mcap_now × entry/now`
    /// (supply ≈ constant). `None` when unpriced or no cost basis.
    pub entry_mcap_usd: Option<String>,
    /// Average entry price per token (net SOL cost basis × SOL price ÷
    /// tokens held). Feeds the break-even stop + entry MCAP.
    pub avg_entry_price_usd: Option<String>,
    /// Live SOL/USD — lets the GUI render every figure in SOL.
    pub sol_price_usd: Option<String>,
    /// Net SOL cost basis (in − out), SOL units.
    pub cost_sol: Option<String>,
    /// Current value in SOL (`value_usd / sol_price`).
    pub value_sol: Option<String>,
    /// Unrealized PnL in SOL (`value_sol − cost_sol`).
    pub pnl_sol: Option<String>,
    /// Cumulative realized PnL banked on this mint (lamports).
    pub realized_pnl_lamports: i64,
    /// Lifetime fill count on the position.
    pub fill_count: i32,
}

/// Market cap at the average entry price: `mcap_now × entry / now`.
/// Token supply is effectively constant for SPL memecoins, so scaling
/// the live cap by the price ratio recovers the entry-time cap without
/// any historical data. `None` unless all three inputs are positive.
fn entry_mcap(
    mcap_now: Option<f64>,
    avg_entry: Option<f64>,
    price_now: Option<f64>,
) -> Option<f64> {
    match (mcap_now, avg_entry, price_now) {
        (Some(m), Some(e), Some(p)) if m > 0.0 && e > 0.0 && p > 0.0 => {
            let v = m * e / p;
            v.is_finite().then_some(v)
        }
        _ => None,
    }
}

const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Same best-effort decimals inference the web frontend uses
/// (`TradingPage.tsx::inferPositionDecimals`) until decimals are
/// plumbed onto the position row end-to-end.
fn infer_decimals(p: &GwPosition) -> i32 {
    if let Some(d) = p.decimals {
        if d >= 0 {
            return d;
        }
    }
    match p.mint.as_str() {
        WSOL => 9,
        USDC => 6,
        _ => 6,
    }
}

pub async fn fetch_positions(auth: &GatewayAuth) -> Result<Vec<SolPositionDto>, String> {
    let rows: Vec<GwPosition> =
        get_json(auth, "/api/trading/positions?only_open=true&limit=200").await?;
    Ok(rows
        .into_iter()
        .map(|p| {
            let decimals = infer_decimals(&p);
            let net_raw = num(&p.net_amount).unwrap_or(0.0);
            let tokens_ui = net_raw / 10f64.powi(decimals);
            let sol_price = p.sol_price_usd.as_ref().and_then(num).filter(|v| *v > 0.0);
            let price = p
                .current_price_usd
                .as_ref()
                .and_then(num)
                .filter(|v| *v > 0.0);
            let net_sol_cost = (p.total_in_lamports - p.total_out_lamports) as f64 / 1e9;
            let cost_usd = sol_price
                .map(|sp| net_sol_cost * sp)
                .filter(|c| c.is_finite() && *c > 0.0);
            let value_usd = price.map(|pr| tokens_ui * pr).filter(|v| v.is_finite());
            let pnl_usd = match (cost_usd, value_usd) {
                (Some(c), Some(v)) => Some(v - c),
                _ => None,
            };
            // ── W3.1 derived figures ─────────────────────────────
            let mcap_now = p
                .current_market_cap_usd
                .as_ref()
                .and_then(num)
                .filter(|v| *v > 0.0);
            let avg_entry = cost_usd
                .filter(|_| tokens_ui > 0.0)
                .map(|c| c / tokens_ui)
                .filter(|v| v.is_finite() && *v > 0.0);
            let entry_cap = entry_mcap(mcap_now, avg_entry, price);
            let cost_sol = Some(net_sol_cost).filter(|c| c.is_finite() && *c > 0.0);
            let value_sol = match (value_usd, sol_price) {
                (Some(v), Some(sp)) => Some(v / sp).filter(|x| x.is_finite()),
                _ => None,
            };
            let pnl_sol = match (cost_sol, value_sol) {
                (Some(c), Some(v)) => Some(v - c),
                _ => None,
            };
            let symbol = p
                .symbol
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| {
                    let m = &p.mint;
                    if m.len() > 8 {
                        format!("{}…", &m[..6])
                    } else {
                        m.clone()
                    }
                });
            SolPositionDto {
                mint: p.mint,
                symbol,
                amount_ui: fmt_amount(tokens_ui),
                cost_usd: cost_usd.map(fmt_usd2),
                value_usd: value_usd.map(fmt_usd2),
                pnl_usd: pnl_usd.map(fmt_usd2),
                source: None,
                opened_at: p.first_fill_at,
                current_price_usd: price.map(|v| format!("{v}")),
                name: p.token_name.filter(|s| !s.trim().is_empty()),
                image_url: p.image_url.filter(|s| !s.trim().is_empty()),
                mcap_usd: mcap_now.map(|v| format!("{v}")),
                entry_mcap_usd: entry_cap.map(|v| format!("{v}")),
                avg_entry_price_usd: avg_entry.map(|v| format!("{v}")),
                sol_price_usd: sol_price.map(|v| format!("{v}")),
                cost_sol: cost_sol.map(|v| format!("{v}")),
                value_sol: value_sol.map(|v| format!("{v}")),
                pnl_sol: pnl_sol.map(|v| format!("{v}")),
                realized_pnl_lamports: p.realized_pnl_lamports,
                fill_count: p.fill_count,
            }
        })
        .collect())
}

// ─── bot sessions ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GwBotSession {
    id: String,
    #[serde(default)]
    preset_id: Option<String>,
    status: String,
    per_trade_lamports: i64,
    budget_lamports: i64,
    spent_lamports: i64,
    fill_count: i32,
    #[serde(default)]
    wallet_pubkey: Option<String>,
    #[serde(default)]
    per_token_cap_lamports: Option<i64>,
    #[serde(default)]
    tip_lamports: Option<i64>,
    #[serde(default)]
    default_ladder: Option<serde_json::Value>,
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GwPreset {
    id: String,
    name: String,
}

#[derive(Debug, Serialize)]
pub struct TpLegDto {
    pub pct: f64,
    pub multiple: f64,
}

/// GUI shape — must match `BotPreset` in `src/ipc.ts`.
#[derive(Debug, Serialize)]
pub struct BotPresetDto {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub chain: &'static str,
    pub buy_sol: String,
    pub budget_sol: String,
    pub spent_sol: String,
    pub tp_ladder: Vec<TpLegDto>,
    pub sl_pct: Option<f64>,
    pub fill_count: i32,
    pub expires_at: Option<String>,
    // Raw fields the in-app arm/clone flows need (display strings
    // above stay for the table).
    pub preset_id: Option<String>,
    pub wallet_pubkey: Option<String>,
    pub per_trade_lamports: i64,
    pub budget_lamports: i64,
    pub spent_lamports: i64,
    pub per_token_cap_lamports: Option<i64>,
    pub tip_lamports: Option<i64>,
}

/// Parse a `targets::LegSpec` array (`[{kind, trigger_pct,
/// sell_fraction_bps}, …]`) into the GUI's ladder notation.
fn parse_ladder(ladder: Option<&serde_json::Value>) -> (Vec<TpLegDto>, Option<f64>) {
    let mut tps = Vec::new();
    let mut sl: Option<f64> = None;
    let Some(serde_json::Value::Array(legs)) = ladder else {
        return (tps, sl);
    };
    for leg in legs {
        let kind = leg.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        let trigger = leg.get("trigger_pct").and_then(num);
        let frac_bps = leg.get("sell_fraction_bps").and_then(num);
        match (kind, trigger) {
            ("tp", Some(t)) => tps.push(TpLegDto {
                pct: frac_bps.map(|b| b / 100.0).unwrap_or(0.0),
                multiple: 1.0 + t / 100.0,
            }),
            ("sl", Some(t)) => {
                if sl.is_none() {
                    sl = Some(t);
                }
            }
            _ => {}
        }
    }
    (tps, sl)
}

pub async fn fetch_bot_sessions(auth: &GatewayAuth) -> Result<Vec<BotPresetDto>, String> {
    let sessions: Vec<GwBotSession> = get_json(auth, "/api/trading/bot/sessions").await?;
    // Best-effort preset-name resolution; a failure degrades to ids,
    // never errors the whole surface.
    let names: HashMap<String, String> =
        match get_json::<Vec<GwPreset>>(auth, "/api/alpha/presets").await {
            Ok(presets) => presets.into_iter().map(|p| (p.id, p.name)).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "preset-name lookup failed — falling back to ids");
                HashMap::new()
            }
        };
    Ok(sessions
        .into_iter()
        .map(|s| {
            let (tp_ladder, sl_pct) = parse_ladder(s.default_ladder.as_ref());
            let name = s
                .preset_id
                .as_ref()
                .map(|pid| {
                    names
                        .get(pid)
                        .cloned()
                        .unwrap_or_else(|| format!("preset {}…", &pid[..8.min(pid.len())]))
                })
                .unwrap_or_else(|| format!("session {}…", &s.id[..8.min(s.id.len())]));
            BotPresetDto {
                id: s.id,
                name,
                enabled: s.status == "active",
                chain: "solana",
                buy_sol: fmt_amount(s.per_trade_lamports as f64 / 1e9),
                budget_sol: fmt_amount(s.budget_lamports as f64 / 1e9),
                spent_sol: fmt_amount(s.spent_lamports as f64 / 1e9),
                tp_ladder,
                sl_pct,
                fill_count: s.fill_count,
                expires_at: s.expires_at,
                preset_id: s.preset_id,
                wallet_pubkey: s.wallet_pubkey,
                per_trade_lamports: s.per_trade_lamports,
                budget_lamports: s.budget_lamports,
                spent_lamports: s.spent_lamports,
                per_token_cap_lamports: s.per_token_cap_lamports,
                tip_lamports: s.tip_lamports,
            }
        })
        .collect())
}

// ─── copytrade configs ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GwSolCopyConfig {
    id: String,
    tracked_wallet_id: String,
    enabled: bool,
    sizing_mode: i16,
    #[serde(default)]
    max_position_sol_lamports: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct GwTrackedWallet {
    id: String,
    address: String,
    #[serde(default)]
    alias: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GwSolCopyIntent {
    config_id: String,
    status: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct GwHlCopyConfig {
    id: String,
    target_wallet: String,
    enabled: bool,
    follow_mode: i16,
    #[serde(default)]
    max_position_usd: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct GwHlCopyIntent {
    config_id: String,
    status: String,
    created_at: DateTime<Utc>,
}

/// GUI shape — must match `CopytradeConfig` in `src/ipc.ts`.
#[derive(Debug, Serialize)]
pub struct CopytradeConfigDto {
    pub id: String,
    pub label: String,
    pub venue: &'static str,
    pub leader: String,
    pub enabled: bool,
    pub size_mode: &'static str,
    pub max_position_usd: Option<String>,
    pub max_position_sol: Option<String>,
    pub copied_24h: i64,
    pub last_copy_at: Option<String>,
}

fn copy_stats(
    intents: &[(String, String, DateTime<Utc>)],
    config_id: &str,
) -> (i64, Option<String>) {
    let cutoff = Utc::now() - Duration::hours(24);
    let mut copied_24h = 0i64;
    let mut last: Option<DateTime<Utc>> = None;
    for (cid, status, at) in intents {
        if cid != config_id || status == "rejected" {
            continue;
        }
        if *at >= cutoff {
            copied_24h += 1;
        }
        if last.is_none_or(|l| *at > l) {
            last = Some(*at);
        }
    }
    (copied_24h, last.map(|t| t.to_rfc3339()))
}

fn short(addr: &str) -> String {
    if addr.len() <= 10 {
        addr.to_string()
    } else {
        format!("{}…{}", &addr[..4], &addr[addr.len() - 4..])
    }
}

pub async fn fetch_copytrade_configs(
    auth: &GatewayAuth,
) -> Result<Vec<CopytradeConfigDto>, String> {
    let mut out = Vec::new();

    // Solana — the Wave-2 engine's configs (`/api/trading/copy/*`).
    let sol_cfgs: Vec<GwSolCopyConfig> = get_json(auth, "/api/trading/copy/configs").await?;
    if !sol_cfgs.is_empty() {
        let wallets: Vec<GwTrackedWallet> = match get_json(auth, "/api/wallet-tracker/wallets")
            .await
        {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "tracked-wallet lookup failed — leaders show as ids");
                Vec::new()
            }
        };
        let by_id: HashMap<&str, &GwTrackedWallet> =
            wallets.iter().map(|w| (w.id.as_str(), w)).collect();
        let intents: Vec<GwSolCopyIntent> =
            match get_json(auth, "/api/trading/copy/intents?limit=200").await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "copy-intent ledger fetch failed — stats blank");
                    Vec::new()
                }
            };
        let flat: Vec<(String, String, DateTime<Utc>)> = intents
            .into_iter()
            .map(|i| (i.config_id, i.status, i.created_at))
            .collect();
        for c in sol_cfgs {
            let wallet = by_id.get(c.tracked_wallet_id.as_str());
            let leader = wallet
                .map(|w| w.address.clone())
                .unwrap_or_else(|| c.tracked_wallet_id.clone());
            let label = wallet
                .and_then(|w| w.alias.clone())
                .filter(|a| !a.trim().is_empty())
                .unwrap_or_else(|| short(&leader));
            let (copied_24h, last_copy_at) = copy_stats(&flat, &c.id);
            out.push(CopytradeConfigDto {
                id: c.id,
                label,
                venue: "solana",
                leader,
                enabled: c.enabled,
                size_mode: if c.sizing_mode == 1 {
                    "pct_balance"
                } else {
                    "fixed_sol"
                },
                max_position_usd: None,
                max_position_sol: c
                    .max_position_sol_lamports
                    .map(|l| fmt_amount(l as f64 / 1e9)),
                copied_24h,
                last_copy_at,
            });
        }
    }

    // Hyperliquid — read-only surfacing of the server-side configs.
    // Failure here must not blank the Solana rows (the HL module may be
    // disabled for the user).
    match get_json::<Vec<GwHlCopyConfig>>(auth, "/api/hyperliquid/copy-trade/configs").await {
        Ok(hl_cfgs) => {
            let intents: Vec<GwHlCopyIntent> =
                (get_json(auth, "/api/hyperliquid/copy-trade/intents").await).unwrap_or_default();
            let flat: Vec<(String, String, DateTime<Utc>)> = intents
                .into_iter()
                .map(|i| (i.config_id, i.status, i.created_at))
                .collect();
            for c in hl_cfgs {
                let (copied_24h, last_copy_at) = copy_stats(&flat, &c.id);
                out.push(CopytradeConfigDto {
                    id: c.id,
                    label: short(&c.target_wallet),
                    venue: "hyperliquid",
                    leader: c.target_wallet,
                    enabled: c.enabled,
                    size_mode: match c.follow_mode {
                        1 => "fixed_usd",
                        2 => "equity_pct",
                        _ => "mirror_pct",
                    },
                    max_position_usd: c.max_position_usd.as_ref().and_then(num).map(fmt_usd2),
                    max_position_sol: None,
                    copied_24h,
                    last_copy_at,
                });
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "HL copy-config fetch failed — showing Solana only");
        }
    }

    Ok(out)
}

/// Toggle a SOLANA copy config. The backend engine reads `enabled` on
/// every decision, so this takes effect server-side immediately. (HL
/// configs are managed via the web app's follow/unfollow flow — their
/// single-follow invariant is more than a boolean.)
pub async fn set_copy_config_enabled(
    auth: &GatewayAuth,
    config_id: &str,
    enabled: bool,
) -> Result<(), String> {
    let path = format!("/api/trading/copy/configs/{config_id}");
    let url = format!("{}{}", auth.base, path);
    let resp = http()
        .patch(&url)
        .bearer_auth(&auth.token)
        .json(&serde_json::json!({ "enabled": enabled }))
        .send()
        .await
        .map_err(|e| format!("PATCH {path}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("PATCH {path}: gateway {status}: {body}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_parses_tp_and_sl_legs() {
        let ladder = serde_json::json!([
            { "kind": "tp", "trigger_pct": "100", "sell_fraction_bps": 5000 },
            { "kind": "tp", "trigger_pct": "400", "sell_fraction_bps": 2500 },
            { "kind": "sl", "trigger_pct": "40", "sell_fraction_bps": 10000 },
        ]);
        let (tps, sl) = parse_ladder(Some(&ladder));
        assert_eq!(tps.len(), 2);
        assert_eq!(tps[0].pct, 50.0);
        assert_eq!(tps[0].multiple, 2.0);
        assert_eq!(tps[1].pct, 25.0);
        assert_eq!(tps[1].multiple, 5.0);
        assert_eq!(sl, Some(40.0));
        // Missing/empty ladders degrade to nothing armed.
        let (tps, sl) = parse_ladder(None);
        assert!(tps.is_empty() && sl.is_none());
    }

    #[test]
    fn copy_stats_counts_recent_non_rejected_only() {
        let now = Utc::now();
        let rows = vec![
            (
                "a".to_string(),
                "published".to_string(),
                now - Duration::hours(1),
            ),
            (
                "a".to_string(),
                "rejected".to_string(),
                now - Duration::hours(2),
            ),
            (
                "a".to_string(),
                "published".to_string(),
                now - Duration::hours(30),
            ),
            ("b".to_string(), "published".to_string(), now),
        ];
        let (n, last) = copy_stats(&rows, "a");
        assert_eq!(n, 1); // 1h-ago row only (30h is outside the window)
                          // last_copy_at is the newest non-rejected row regardless of window.
        assert!(last.is_some());
        let (n_b, _) = copy_stats(&rows, "b");
        assert_eq!(n_b, 1);
    }

    #[test]
    fn entry_mcap_scales_live_cap_by_price_ratio() {
        // mcap $1M, avg entry $0.0005, now $0.001 → entry cap $500K.
        let v = entry_mcap(Some(1_000_000.0), Some(0.0005), Some(0.001)).unwrap();
        assert!((v - 500_000.0).abs() < 1e-6);
        // Any missing / non-positive input → None (never NaN/inf).
        assert_eq!(entry_mcap(None, Some(1.0), Some(1.0)), None);
        assert_eq!(entry_mcap(Some(1.0), Some(0.0), Some(1.0)), None);
        assert_eq!(entry_mcap(Some(1.0), Some(1.0), None), None);
    }

    #[test]
    fn num_reads_strings_and_numbers() {
        assert_eq!(num(&serde_json::json!("1.5")), Some(1.5));
        assert_eq!(num(&serde_json::json!(2)), Some(2.0));
        assert_eq!(num(&serde_json::json!(null)), None);
    }

    #[test]
    fn needs_refresh_inside_window_or_unknown_only() {
        let now = Utc::now();
        let fresh = (now + Duration::hours(20)).to_rfc3339();
        let stale = (now + Duration::hours(2)).to_rfc3339();
        let past = (now - Duration::hours(1)).to_rfc3339();
        assert!(!needs_refresh(Some(&fresh), now), "20h left: leave it");
        assert!(needs_refresh(Some(&stale), now), "2h left: renew");
        assert!(needs_refresh(Some(&past), now), "past exp: attempt");
        assert!(needs_refresh(None, now), "unknown exp: learn it");
        assert!(needs_refresh(Some("garbage"), now), "unparseable: learn it");
    }

    #[test]
    fn jwt_exp_decodes_without_verification() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header = b64.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = b64.encode(br#"{"sub":"x","exp":1750000000}"#);
        let token = format!("{header}.{payload}.not-a-real-sig");
        let exp = jwt_exp_rfc3339(&token).expect("exp decodes");
        assert!(exp.starts_with("2025-06-15T")); // 1750000000 = 2025-06-15 UTC
                                                 // Malformed tokens degrade to None, never panic.
        assert_eq!(jwt_exp_rfc3339("nodots"), None);
        assert_eq!(jwt_exp_rfc3339("a.b.c"), None);
        let no_exp = format!("{header}.{}.sig", b64.encode(br#"{"sub":"x"}"#));
        assert_eq!(jwt_exp_rfc3339(&no_exp), None);
    }
}
