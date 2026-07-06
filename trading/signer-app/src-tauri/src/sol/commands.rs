//! Tauri IPC commands for the Solana surfaces (Wave-4 wiring).
//!
//! Read commands resolve gateway auth per-call (`gateway::resolve_auth`)
//! so they work as soon as EITHER the HL pairing token exists or the
//! web app pushed a session token to the `:5829` daemon. Errors are
//! returned verbatim — the GUI shows them in the standard error box
//! instead of silently degrading.

use crate::sol::config::SolConfig;
use crate::sol::gateway::{self, BotPresetDto, CopytradeConfigDto, SolPositionDto};
use crate::sol::runtime::{self, SolRuntimeStatus};
use crate::state::AppState;
use degenbox_signer_core as core;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use tauri::State;

// ─── positions / balance ───────────────────────────────────────────

#[tauri::command]
pub async fn sol_positions(state: State<'_, AppState>) -> Result<Vec<SolPositionDto>, String> {
    let auth = gateway::resolve_auth(&state).await?;
    gateway::fetch_positions(&auth).await
}

/// Gateway route prefixes the webview proxy may touch (audit N5).
/// Data surfaces ONLY — `/api/auth/*`, `/api/admin/*` and everything
/// else stay unreachable from webview-driven code even though the
/// desktop bearer would authorize more (defense-in-depth: an XSS or a
/// compromised webview dependency must not get full account-API
/// control). Extend ONLY for prefixes a GUI surface actually calls —
/// the pinned call-site inventory is the
/// `gateway_fetch_allows_every_current_call_site` test below.
const GATEWAY_FETCH_PREFIXES: &[&str] = &[
    "/api/trading/",        // positions, pnl, intents, bot sessions, copy-trade
    "/api/hyperliquid/",    // perps positions, pnl, candles, copy-trade
    "/api/alpha/",          // presets, token history/backfill
    "/api/exec/",           // execution-computer subscriptions/instructions
    "/api/signals/parser/", // caller list for the follow picker
    "/api/wallet-tracker/", // track a pasted copy-trade leader
];

/// Methods the proxy forwards. PUT is deliberately absent — no GUI
/// surface uses it (`lib/gateway.ts` exposes gwGet/gwPost/gwPatch/
/// gwDelete only).
const GATEWAY_FETCH_METHODS: &[&str] = &["GET", "POST", "PATCH", "DELETE"];

/// Validate a `gateway_fetch` path: explicit prefix allowlist plus
/// shape hardening. The route part (before `?`) must be plain ASCII
/// with no `//`, no percent-escapes (`%2e%2e` dies here) and no
/// backslashes; `..` and whitespace/control chars are rejected in the
/// WHOLE string. Query strings keep their percent-encoded params.
fn gateway_path_allowed(path: &str) -> Result<(), String> {
    let reject = || Err(format!("gateway_fetch: path not allowed: {path}"));
    if path.contains("..") || path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return reject();
    }
    let route = path.split('?').next().unwrap_or("");
    if route.contains("//") || route.contains('%') || route.contains('\\') || !route.is_ascii() {
        return reject();
    }
    if !GATEWAY_FETCH_PREFIXES.iter().any(|p| route.starts_with(p)) {
        return Err(format!(
            "gateway_fetch: path not allowed: {path} (allowed prefixes: {})",
            GATEWAY_FETCH_PREFIXES.join(", ")
        ));
    }
    Ok(())
}

/// Generic authed gateway proxy for GUI data surfaces. Restricted to
/// the explicit data-route allowlist above — auth flows stay in
/// `auth.rs`, signing stays local. Lets new read/CRUD endpoints land
/// in the GUI without a bespoke Rust DTO per route.
#[tauri::command]
pub async fn gateway_fetch(
    state: State<'_, AppState>,
    method: String,
    path: String,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    if !GATEWAY_FETCH_METHODS.contains(&method.as_str()) {
        return Err(format!("gateway_fetch: method not allowed: {method}"));
    }
    gateway_path_allowed(&path)?;
    let auth = gateway::resolve_auth(&state).await?;
    gateway::request_json_raw(&auth, &method, &path, body).await
}

#[derive(Debug, Serialize)]
pub struct SolWalletBalanceDto {
    pub sol_ui: String,
    /// USD valuation needs a SOL price feed the signer doesn't carry —
    /// deliberately `None`; the GUI omits the ≈ line.
    pub usd_value: Option<String>,
}

/// Native SOL balance of a local wallet, read straight from the
/// configured RPC. Works while locked — pubkeys are public (vault
/// manifest / keystore JSON). `pubkey` selects any wallet (the fleet
/// UI passes each client's address); default = the primary.
#[tauri::command]
pub async fn sol_wallet_balance(
    pubkey: Option<String>,
    state: State<'_, AppState>,
) -> Result<SolWalletBalanceDto, String> {
    let pubkey_b58 = match pubkey.filter(|p| !p.trim().is_empty()) {
        Some(p) => p.trim().to_string(),
        None => crate::clients::primary_sol_pubkey()
            .ok_or_else(|| "no Solana wallet set up yet".to_string())?,
    };
    let pubkey = solana_sdk::pubkey::Pubkey::from_str(&pubkey_b58)
        .map_err(|e| format!("wallet pubkey invalid: {e}"))?;
    // Gateway RPC proxy default (slice 8); legacy chain when no
    // credentials resolve (public endpoint still serves a balance read).
    let auth = gateway::resolve_auth(&state).await.ok();
    let rpc = core::RpcClient::new(
        SolConfig::load_or_default()
            .resolved_rpc_url_with_auth(auth.as_ref().map(|a| (a.base.as_str(), a.token.as_str()))),
    );
    let lamports = rpc
        .get_balance(&pubkey)
        .await
        .map_err(|e| format!("balance fetch: {e}"))?;
    Ok(SolWalletBalanceDto {
        sol_ui: format!("{:.4}", lamports as f64 / 1e9),
        usd_value: None,
    })
}

// ─── bots / copytrade ──────────────────────────────────────────────

#[tauri::command]
pub async fn bot_presets_status(state: State<'_, AppState>) -> Result<Vec<BotPresetDto>, String> {
    let auth = gateway::resolve_auth(&state).await?;
    gateway::fetch_bot_sessions(&auth).await
}

#[tauri::command]
pub async fn copytrade_configs(
    state: State<'_, AppState>,
) -> Result<Vec<CopytradeConfigDto>, String> {
    let auth = gateway::resolve_auth(&state).await?;
    gateway::fetch_copytrade_configs(&auth).await
}

/// Enable/pause a SOLANA copy config (server-side PATCH — the backend
/// engine is the policy layer and reads `enabled` on every decision).
#[tauri::command]
pub async fn copytrade_set_enabled(
    config_id: String,
    enabled: bool,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let auth = gateway::resolve_auth(&state).await?;
    gateway::set_copy_config_enabled(&auth, &config_id, enabled).await
}

// ─── runtime status + execution config ─────────────────────────────

#[tauri::command]
pub fn sol_runtime_status(state: State<'_, AppState>) -> Result<SolRuntimeStatus, String> {
    Ok(state.sol_runtime.snapshot())
}

#[derive(Debug, Serialize)]
pub struct SolExecConfigDto {
    /// RETIRED (slice 8) — kept for the current UI until the slice-9
    /// redesign binds; no longer gates anything.
    pub copy_session_sol: Option<f64>,
    /// RETIRED (slice 8) — see above.
    pub copy_per_token_sol: Option<f64>,
    /// Effective RPC URL (override → `SOLANA_RPC_URL` → gateway RPC
    /// proxy → public default).
    pub rpc_url: String,
    /// The explicit config override, when set (the UI's input value;
    /// `rpc_url` above is its placeholder when unset).
    pub rpc_url_override: Option<String>,
    /// What `rpc_url` resolves to WITHOUT the override — the UI shows
    /// it as the placeholder. With gateway credentials this is the
    /// token-gated proxy; shown with the token REDACTED (it's a
    /// display string, not a connect target).
    pub rpc_url_default: String,
    pub slippage_bps: u16,
    pub tip_lamports: i64,
    pub submit_mode: String,
}

#[tauri::command]
pub async fn sol_exec_config_get(state: State<'_, AppState>) -> Result<SolExecConfigDto, String> {
    let cfg = SolConfig::load_or_default();
    let auth = gateway::resolve_auth(&state).await.ok();
    let auth_pair = auth.as_ref().map(|a| (a.base.as_str(), a.token.as_str()));
    let default_rpc = SolConfig {
        rpc_url: None,
        ..cfg.clone()
    }
    .resolved_rpc_url_with_auth(auth_pair.map(|(base, _)| (base, "REDACTED")));
    Ok(SolExecConfigDto {
        copy_session_sol: cfg.copy_session_sol,
        copy_per_token_sol: cfg.copy_per_token_sol,
        rpc_url: cfg.resolved_rpc_url_with_auth(auth_pair.map(|(base, _)| (base, "REDACTED"))),
        rpc_url_override: cfg.rpc_url.clone(),
        rpc_url_default: default_rpc,
        slippage_bps: cfg.slippage_bps,
        tip_lamports: cfg.tip_lamports,
        submit_mode: cfg.submit_mode,
    })
}

#[derive(Debug, Deserialize)]
pub struct SolExecConfigReq {
    pub copy_session_sol: Option<f64>,
    pub copy_per_token_sol: Option<f64>,
}

/// Persist the legacy client-budget fields. RETIRED as a gate since
/// slice 8 (copy budgets are per-config server-side); kept only so the
/// current UI's inputs don't error until the slice-9 redesign removes
/// them. Writing here changes nothing at execution time.
#[tauri::command]
pub fn sol_exec_config_set(req: SolExecConfigReq, app: tauri::AppHandle) -> Result<(), String> {
    for v in [req.copy_session_sol, req.copy_per_token_sol]
        .into_iter()
        .flatten()
    {
        if !v.is_finite() || v < 0.0 {
            return Err("budget values must be positive numbers".into());
        }
    }
    let mut cfg = SolConfig::load_or_default();
    cfg.copy_session_sol = req.copy_session_sol.filter(|v| *v > 0.0);
    cfg.copy_per_token_sol = req.copy_per_token_sol.filter(|v| *v > 0.0);
    cfg.save()?;
    // Apply live: `spawn` signals the old loop to stop, waits for it to
    // release the running guard, then starts fresh with the new
    // BudgetState. No-op while locked (refuses without an unlocked
    // keypair).
    runtime::spawn(&app);
    Ok(())
}

/// Execution-parameter update (M23) — RPC URL, default slippage, tip,
/// submit mode. Deliberately a SEPARATE command from the budget setter:
/// the budget contract treats absent fields as "clear/disarm", so
/// folding exec params into it would let either card clobber the
/// other's settings. `None`/empty here = reset that field to its
/// default.
#[derive(Debug, Deserialize)]
pub struct SolExecParamsReq {
    /// Empty/None = drop the override (fall back to `SOLANA_RPC_URL`,
    /// then public mainnet-beta).
    pub rpc_url: Option<String>,
    /// None = default (100 bps).
    pub slippage_bps: Option<u16>,
    /// None = default (1_000_000 lamports).
    pub tip_lamports: Option<i64>,
    /// None = default ("falcon_jito"). Must be one of
    /// `falcon_jito | quic | tpu`.
    pub submit_mode: Option<String>,
}

/// Validate a [`SolExecParamsReq`] into the normalized (override,
/// slippage, tip, submit_mode) tuple. Pure — unit-tested directly.
#[allow(clippy::type_complexity)]
pub(crate) fn validate_exec_params(
    req: &SolExecParamsReq,
) -> Result<(Option<String>, u16, i64, String), String> {
    let rpc_url = req
        .rpc_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(u) = &rpc_url {
        if !(u.starts_with("http://") || u.starts_with("https://")) {
            return Err("RPC URL must start with http:// or https://".into());
        }
    }
    let defaults = SolConfig::default();
    let slippage_bps = match req.slippage_bps {
        None => defaults.slippage_bps,
        Some(0) => return Err("slippage must be at least 1 bps".into()),
        Some(bps) if bps > 10_000 => return Err("slippage cannot exceed 10000 bps (100%)".into()),
        Some(bps) => bps,
    };
    let tip_lamports = match req.tip_lamports {
        None => defaults.tip_lamports,
        Some(t) if t < 0 => return Err("tip must be ≥ 0 lamports".into()),
        // 0.1 SOL tip ceiling — a fat-finger guard, not a policy.
        Some(t) if t > 100_000_000 => {
            return Err("tip above 0.1 SOL (100000000 lamports) — almost certainly a typo".into())
        }
        Some(t) => t,
    };
    let submit_mode = match req.submit_mode.as_deref().map(str::trim) {
        None | Some("") => defaults.submit_mode,
        Some(m @ ("falcon_jito" | "quic" | "tpu")) => m.to_string(),
        Some(other) => {
            return Err(format!(
                "unknown submit mode {other:?} (falcon_jito | quic | tpu)"
            ))
        }
    };
    Ok((rpc_url, slippage_bps, tip_lamports, submit_mode))
}

/// Persist the execution params and restart the Solana runtime so the
/// engines (which capture rpc/slippage/tip/submit at build time) pick
/// them up immediately. Budgets are untouched.
#[tauri::command]
pub fn sol_exec_params_set(req: SolExecParamsReq, app: tauri::AppHandle) -> Result<(), String> {
    let (rpc_url, slippage_bps, tip_lamports, submit_mode) = validate_exec_params(&req)?;
    let mut cfg = SolConfig::load_or_default();
    cfg.rpc_url = rpc_url;
    cfg.slippage_bps = slippage_bps;
    cfg.tip_lamports = tip_lamports;
    cfg.submit_mode = submit_mode;
    cfg.save()?;
    runtime::spawn(&app);
    Ok(())
}

// ─── keystore migration / import ───────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CliKeystoreInfo {
    pub path: String,
    pub pubkey: String,
}

fn cli_default_keystore_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".degenbox").join("keystore.json"))
}

fn peek_pubkey(path: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let ks: core::Keystore = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a DegenBox signer keystore: {e}", path.display()))?;
    Ok(ks.pubkey)
}

/// Detect the signer-cli's default keystore (`~/.degenbox/keystore.json`)
/// so Settings can offer a one-click migration. `null` when absent or
/// unreadable.
#[tauri::command]
pub fn detect_cli_keystore() -> Result<Option<CliKeystoreInfo>, String> {
    let Some(path) = cli_default_keystore_path() else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    match peek_pubkey(&path) {
        Ok(pubkey) => Ok(Some(CliKeystoreInfo {
            path: path.to_string_lossy().into_owned(),
            pubkey,
        })),
        Err(e) => {
            tracing::warn!(error = %e, "CLI keystore present but unreadable");
            Ok(None)
        }
    }
}

/// Import an existing signer-cli / signer-core keystore FILE. Same
/// encrypted format on both sides, so this is a validate-then-copy —
/// the password is never needed and never touches this command. The
/// original file is left untouched.
#[tauri::command]
pub fn import_sol_keystore_file(path: String) -> Result<CliKeystoreInfo, String> {
    let src = PathBuf::from(path);
    let pubkey = peek_pubkey(&src)?;
    let dest = core::sol_keystore_path().map_err(|e| e.to_string())?;
    if dest.exists() {
        return Err(
            "a Solana keystore already exists in this app — remove it first to replace it".into(),
        );
    }
    // Re-serialise through the typed struct (not a raw byte copy) so a
    // file with trailing garbage or odd permissions normalises, then
    // save with 0600.
    let bytes = std::fs::read(&src).map_err(|e| e.to_string())?;
    let ks: core::Keystore = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    core::save_to_path(&ks, &dest).map_err(|e| e.to_string())?;
    Ok(CliKeystoreInfo {
        path: dest.to_string_lossy().into_owned(),
        pubkey,
    })
}

#[derive(Debug, Deserialize)]
pub struct ImportExtensionReq {
    /// The extension keystore JSON blob (chrome.storage export).
    pub json: String,
    /// The password it was encrypted under — reused for the native
    /// keystore so the user keeps one passphrase.
    pub password: String,
}

/// Import a keystore exported from the DegenBox Chrome extension
/// (different JSON shape, same crypto). Decrypts with the supplied
/// password and adopts it into the VAULT (vault-append — re-runnable;
/// the password must match the vault's master password).
#[tauri::command]
pub fn import_extension_keystore(
    req: ImportExtensionReq,
) -> Result<crate::commands::GenerateSolanaResult, String> {
    let (ks, kp) =
        core::import_extension_json(&req.json, &req.password).map_err(|e| e.to_string())?;
    drop(kp); // wipe
    let mut vault = crate::clients::open_or_create_vault_migrated(&req.password)?;
    let entry = vault
        .adopt_sol_keystore(&ks, &req.password, None)
        .map_err(|e| e.to_string())?;
    Ok(crate::commands::GenerateSolanaResult {
        pubkey: entry.address,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(
        rpc: Option<&str>,
        bps: Option<u16>,
        tip: Option<i64>,
        mode: Option<&str>,
    ) -> SolExecParamsReq {
        SolExecParamsReq {
            rpc_url: rpc.map(str::to_string),
            slippage_bps: bps,
            tip_lamports: tip,
            submit_mode: mode.map(str::to_string),
        }
    }

    #[test]
    fn exec_params_defaults_when_unset() {
        let (rpc, bps, tip, mode) = validate_exec_params(&req(None, None, None, None)).unwrap();
        assert_eq!(rpc, None);
        assert_eq!(bps, 100);
        assert_eq!(tip, 1_000_000);
        assert_eq!(mode, "falcon_jito");
        // Empty strings are "unset" too (the UI sends the raw inputs).
        let (rpc, _, _, mode) =
            validate_exec_params(&req(Some("   "), None, None, Some(""))).unwrap();
        assert_eq!(rpc, None);
        assert_eq!(mode, "falcon_jito");
    }

    #[test]
    fn exec_params_accepts_valid_overrides() {
        let (rpc, bps, tip, mode) = validate_exec_params(&req(
            Some("https://rpc.example.com "),
            Some(250),
            Some(2_000_000),
            Some("quic"),
        ))
        .unwrap();
        assert_eq!(rpc.as_deref(), Some("https://rpc.example.com"));
        assert_eq!(bps, 250);
        assert_eq!(tip, 2_000_000);
        assert_eq!(mode, "quic");
        assert!(validate_exec_params(&req(None, None, None, Some("tpu"))).is_ok());
    }

    /// Every path a GUI surface actually sends through the proxy today
    /// must pass — pin them so an allowlist edit can't silently kill a
    /// tab. This list IS the call-site inventory (sources annotated).
    #[test]
    fn gateway_fetch_allows_every_current_call_site() {
        for p in [
            // features/positions/data.ts
            "/api/trading/pnl/windows",
            "/api/trading/intents",
            "/api/alpha/tokens/1399811149/Mint111/history?interval_secs=60&limit=300&before=2026-06-12T00%3A00%3A00Z",
            "/api/alpha/tokens/1399811149/Mint111/backfill",
            // features/presets/ipc.ts
            "/api/alpha/presets",
            "/api/alpha/presets/0e1f2a3b",
            "/api/trading/copy-trade/summary",
            "/api/trading/copy-trade/configs/abc/follow",
            "/api/wallet-tracker/wallets",
            // features/perps-positions/data.ts
            "/api/hyperliquid/pnl/windows",
            "/api/hyperliquid/wallets/0xabc/positions",
            "/api/hyperliquid/candles/BTC?interval=1m&start=0&end=1",
            // features/perps-presets/ipc.ts
            "/api/exec/subscriptions",
            "/api/exec/subscriptions/42",
            "/api/exec/instructions",
            "/api/signals/parser/callers",
            "/api/hyperliquid/copy-trade/summary",
            "/api/hyperliquid/copy-trade/configs/abc/follow",
            "/api/hyperliquid/copy-trade/configs/abc",
        ] {
            assert!(gateway_path_allowed(p).is_ok(), "should allow {p}");
        }
    }

    #[test]
    fn gateway_fetch_rejects_off_allowlist_routes() {
        for p in [
            "/api/admin/users",           // admin surface (the N5 headline)
            "/api/auth/desktop/exchange", // auth flows stay in auth.rs
            "/api/signals/ingest",        // only parser/ is allowlisted
            "/api/",
            "/api/trading", // no trailing slash → not the prefix
            "/auth/me",     // not even /api/
            "",
        ] {
            assert!(gateway_path_allowed(p).is_err(), "should reject {p}");
        }
    }

    #[test]
    fn gateway_fetch_rejects_path_tricks() {
        for p in [
            "/api/trading/../admin/users",     // dot-dot traversal
            "/api/trading/%2e%2e/admin/users", // percent-encoded traversal
            "/api/trading//intents",           // double slash
            "/api/trading/\\admin",            // backslash
            "/api/trading/in tents",           // whitespace
            "/api/trading/intents\n",          // control char
            "/api/trading/intents?x=..",       // .. hides in the query
            "/api/trading/üintents",           // non-ASCII route
            "/api/trading/%61dmin",            // percent-escape in route
        ] {
            assert!(gateway_path_allowed(p).is_err(), "should reject {p:?}");
        }
        // Percent-encoding stays legal in the QUERY (encoded params).
        assert!(
            gateway_path_allowed("/api/trading/intents?before=2026-06-12T00%3A00%3A00Z").is_ok()
        );
    }

    #[test]
    fn exec_params_rejects_garbage() {
        assert!(validate_exec_params(&req(Some("rpc.example.com"), None, None, None)).is_err());
        assert!(validate_exec_params(&req(None, Some(0), None, None)).is_err());
        assert!(validate_exec_params(&req(None, Some(10_001), None, None)).is_err());
        assert!(validate_exec_params(&req(None, None, Some(-1), None)).is_err());
        assert!(validate_exec_params(&req(None, None, Some(200_000_000), None)).is_err());
        assert!(validate_exec_params(&req(None, None, None, Some("jito"))).is_err());
    }
}
