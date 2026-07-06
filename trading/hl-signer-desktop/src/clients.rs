//! Multi-wallet client management — the CLI side of the shared vault.
//!
//! One user → N wallets ("clients"): multiple Solana AND multiple
//! Hyperliquid wallets, all keys under ONE master password in
//! `~/.config/degenbox/vault/` (`degenbox_signer_core::vault`) — the
//! SAME directory, manifest and keystore files the Tauri desktop app
//! uses, so app + CLI on one machine see one wallet fleet.
//!
//! This module owns:
//!
//! - vault helpers (open / open-or-create with legacy migration /
//!   primary resolution) shared by the headless commands and the TUI,
//! - the gateway `/api/trading/clients` client (graceful-degrade: 404
//!   means the endpoint hasn't shipped — local-only view),
//! - the `clients …` headless subcommands (list / add / import /
//!   remove / label / pause / resume / set-primary),
//! - the `run` multi-client runtime: the headless equivalent of the
//!   unlocked desktop app. Topology mirrors the app
//!   (`signer-app/src-tauri/src/clients.rs`):
//!   * HL primary → full poll/sign/report daemon, claim-scoped on its
//!     master wallet with `allow_unstamped` (it owns legacy rows).
//!   * HL secondaries → capability probe
//!     ([`spawn_hl_secondary`]): multi-client gateway → full daemon
//!     with a STRICT per-wallet claim scope (`?wallet=` + the core
//!     `ClaimScope` belt) and its own executed ledger; older gateway /
//!     unpaired → heartbeat + balance standby (never claims — an old
//!     gateway ignores the `?wallet=` filter, so a scoped secondary
//!     poll would steal the primary's instructions).
//!   * Sol primary → the sell+copy engine (events stamped for OTHER
//!     wallets are skipped via `wallet_event_is_mine` — never executed
//!     on the wrong wallet). Per-wallet Sol engines (the app's N-engine
//!     dispatcher) are the one remaining follow-up; see the report.

use crate::branding;
use crate::config;
use anyhow::{anyhow, Context, Result};
use degenbox_signer_core as core;
use degenbox_signer_core::{Vault, WalletChain, WalletEntry};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ─── vault helpers ──────────────────────────────────────────────────

pub fn vault_dir() -> Result<PathBuf, String> {
    core::default_vault_dir().map_err(|e| e.to_string())
}

/// Whether a vault manifest exists on this device.
pub fn vault_exists() -> bool {
    vault_dir().map(|d| Vault::exists(&d)).unwrap_or(false)
}

/// Open the vault if one exists. `Ok(None)` pre-migration.
pub fn open_vault() -> Result<Option<Vault>, String> {
    let dir = vault_dir()?;
    if !Vault::exists(&dir) {
        return Ok(None);
    }
    Vault::open(&dir).map(Some).map_err(|e| e.to_string())
}

/// Open-or-create the vault under `password` and adopt any legacy
/// single-file keystores (renamed to `.bak`, ciphertext preserved —
/// signer-core's `migrate_legacy`, identical to the app's unlock path).
/// Legacy `bots/<name>/` hub clients are deliberately NOT migrated:
/// they are separate gateway pairings (often separate accounts) and
/// keep working through the existing hub flow.
pub fn open_or_create_vault_migrated(password: &str) -> Result<Vault, String> {
    let dir = vault_dir()?;
    let mut vault = Vault::open_or_create(&dir, password).map_err(|e| e.to_string())?;
    vault.verify_password(password).map_err(|e| e.to_string())?;
    let sol_legacy = core::sol_keystore_path().map_err(|e| e.to_string())?;
    let hl_legacy = core::hl_keystore_path().map_err(|e| e.to_string())?;
    let hl_cfg = core::hl_config_path().map_err(|e| e.to_string())?;
    let report = vault
        .migrate_legacy(&sol_legacy, &hl_legacy, Some(&hl_cfg), password)
        .map_err(|e| format!("legacy keystore migration: {e}"))?;
    if report.migrated_sol.is_some() || report.migrated_hl.is_some() {
        tracing::info!(
            sol = ?report.migrated_sol,
            hl = ?report.migrated_hl,
            "migrated legacy keystores into the vault (originals kept as .bak)"
        );
    }
    for note in &report.notes {
        tracing::warn!(%note, "vault migration note");
    }
    Ok(vault)
}

/// The vault primary Solana wallet's keystore file, when a vault with
/// a Sol wallet exists. Lets every legacy single-keystore surface
/// read-through to the vault after migration.
pub fn vault_primary_sol_keystore() -> Option<PathBuf> {
    let v = open_vault().ok()??;
    let w = v.primary(WalletChain::Sol)?;
    let p = v.keystore_path(w);
    p.exists().then_some(p)
}

/// The vault primary HL wallet's keystore file, when present.
pub fn vault_primary_hl_keystore() -> Option<PathBuf> {
    let v = open_vault().ok()??;
    let w = v.primary(WalletChain::Hl)?;
    let p = v.keystore_path(w);
    p.exists().then_some(p)
}

/// The HL pairing config for a vault wallet: its per-wallet config when
/// present, else (primary only) the legacy global `hl-config.json`.
/// Mirrors the app's `hl_config_for`.
pub fn hl_config_for(vault: &Vault, entry: &WalletEntry, is_primary: bool) -> config::Config {
    let per_wallet = vault.hl_config_path(entry);
    if per_wallet.exists() {
        return config::Config::load_from(&per_wallet);
    }
    if is_primary {
        return config::Config::load_or_default();
    }
    config::Config::default()
}

/// Executed-marker ledger path for a vault HL wallet, seeded ONCE from
/// the legacy global `executed.jsonl` so the migration can never reopen
/// the re-submit window (same invariant as the app's
/// `hl_executed_path_seeded`).
pub fn hl_executed_path_seeded(vault: &Vault, entry: &WalletEntry) -> PathBuf {
    let p = vault.hl_executed_path(entry);
    if !p.exists() {
        if let Ok(global) = core::hl::config::executed_path() {
            if global.exists() {
                if let Err(e) = std::fs::copy(&global, &p) {
                    tracing::warn!(error = %e,
                        "could not seed per-wallet executed ledger from the global one");
                }
            }
        }
    }
    p
}

// ─── master password ────────────────────────────────────────────────

/// Master-password sources, following the binary's existing patterns:
/// `--password-stdin` → first stdin line; else `DEGENBOX_MASTER_PASSWORD`
/// env (systemd `EnvironmentFile=` installs); else a hidden prompt.
pub fn read_master_password(password_stdin: bool) -> Result<String> {
    if password_stdin {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        return Ok(buf.trim_end_matches(['\r', '\n']).to_string());
    }
    if let Ok(p) = std::env::var("DEGENBOX_MASTER_PASSWORD") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    let prompt = format!("  {} Master password: ", branding::prefix());
    Ok(rpassword::prompt_password(&prompt)?)
}

/// Like [`read_master_password`], but confirm-prompted when this call
/// will CREATE the vault (a typo'd creation password is unrecoverable).
fn read_master_password_for_append(password_stdin: bool) -> Result<String> {
    let creating = !Vault::exists(&vault_dir().map_err(|e| anyhow!(e))?);
    if password_stdin || !creating || std::env::var("DEGENBOX_MASTER_PASSWORD").is_ok() {
        return read_master_password(password_stdin);
    }
    let p1 = rpassword::prompt_password(format!(
        "  {} New master password (≥ 8 chars): ",
        branding::prefix()
    ))?;
    if p1.len() < 8 {
        return Err(anyhow!("master password must be at least 8 characters"));
    }
    let p2 = rpassword::prompt_password(format!(
        "  {} Confirm master password:          ",
        branding::prefix()
    ))?;
    if p1 != p2 {
        return Err(anyhow!("passwords don't match"));
    }
    Ok(p1)
}

// ─── gateway clients API (graceful-degrade) ─────────────────────────

/// One row from `GET /api/trading/clients`. Deliberately lenient —
/// every field except `id` is defaulted so additive gateway changes
/// never break us (same shape the app carries).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GatewayClient {
    pub id: String,
    #[serde(default)]
    pub chain: Option<String>,
    #[serde(default)]
    pub wallet: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub paused: Option<bool>,
    #[serde(default)]
    pub budget: Option<serde_json::Value>,
    #[serde(default)]
    pub active_config_count: Option<i64>,
    #[serde(default)]
    pub assignment_count: Option<i64>,
    #[serde(default)]
    pub open_positions: Option<i64>,
    #[serde(default)]
    pub unrealized_pnl: Option<serde_json::Value>,
    #[serde(default)]
    pub last_activity: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GatewayAuth {
    pub base: String,
    pub token: String,
}

/// Resolve gateway credentials for the clients API: the Discord
/// desktop login first (`desktop-auth.json`, shared with the app),
/// else the HL pairing JWT in `hl-config.json`.
pub fn gateway_auth() -> Option<GatewayAuth> {
    if let Some(a) = crate::auth::DesktopAuth::load_valid() {
        return Some(GatewayAuth {
            base: a.gateway_base.trim_end_matches('/').to_string(),
            token: a.token,
        });
    }
    let cfg = config::Config::load_or_default();
    cfg.api_token.clone().map(|token| GatewayAuth {
        base: cfg.server_url.trim_end_matches('/').to_string(),
        token,
    })
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("reqwest client")
}

/// Fetch the gateway's client rows. `Ok(None)` when the endpoint
/// doesn't exist yet (404/405) — callers degrade to the local view.
pub async fn fetch_gateway_clients(
    auth: &GatewayAuth,
) -> Result<Option<Vec<GatewayClient>>, String> {
    let url = format!("{}/api/trading/clients", auth.base);
    let resp = http()
        .get(&url)
        .bearer_auth(&auth.token)
        .send()
        .await
        .map_err(|e| format!("GET /api/trading/clients: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 404 || status.as_u16() == 405 {
        return Ok(None);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GET /api/trading/clients: {status}: {body}"));
    }
    // The gateway wraps the rows (`{"clients":[…]}`); the app tolerates
    // both the wrapped and the bare-array shape — so do we.
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GET /api/trading/clients decode: {e}"))?;
    let rows = match body {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(mut o) => match o.remove("clients") {
            Some(serde_json::Value::Array(a)) => a,
            _ => return Err("GET /api/trading/clients: unexpected body shape".into()),
        },
        _ => return Err("GET /api/trading/clients: unexpected body shape".into()),
    };
    let parsed = rows
        .into_iter()
        .filter_map(|v| serde_json::from_value::<GatewayClient>(v).ok())
        .collect();
    Ok(Some(parsed))
}

/// Register a local wallet server-side (drift repair). Best-effort.
async fn register_gateway_client(auth: &GatewayAuth, entry: &WalletEntry) -> Result<(), String> {
    let url = format!("{}/api/trading/clients", auth.base);
    let chain = match entry.chain {
        WalletChain::Sol => "solana",
        WalletChain::Hl => "hyperliquid",
    };
    let resp = http()
        .post(&url)
        .bearer_auth(&auth.token)
        .json(&serde_json::json!({
            "chain": chain,
            "wallet": entry.address,
            "wallet_address": entry.address,
            "label": entry.label,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{status}"));
    }
    Ok(())
}

/// Push a pause toggle server-side. Best-effort.
pub async fn pause_gateway_client(
    auth: &GatewayAuth,
    gateway_id: &str,
    paused: bool,
) -> Result<(), String> {
    let url = format!("{}/api/trading/clients/{gateway_id}/pause", auth.base);
    let resp = http()
        .post(&url)
        .bearer_auth(&auth.token)
        .json(&serde_json::json!({ "paused": paused }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("{}", resp.status()));
    }
    Ok(())
}

/// Best-effort gateway pause sync for a local wallet (resolves the
/// gateway row by address). Used by the headless pause command and the
/// TUI toggle.
pub async fn push_pause_best_effort(address: String, paused: bool) {
    let Some(auth) = gateway_auth() else { return };
    match fetch_gateway_clients(&auth).await {
        Ok(Some(rows)) => {
            if let Some(gw) = rows.iter().find(|r| {
                r.wallet
                    .as_deref()
                    .is_some_and(|w| w.eq_ignore_ascii_case(&address))
            }) {
                if let Err(e) = pause_gateway_client(&auth, &gw.id, paused).await {
                    tracing::warn!(error = %e, "gateway client pause push failed (local pause holds)");
                }
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "gateway clients fetch failed during pause push"),
    }
}

// ─── merged view ────────────────────────────────────────────────────

/// Merged local + gateway view of one client. Addresses + statuses
/// only — never key material. Field names match the app's `ClientInfo`.
#[derive(Debug, Serialize)]
pub struct ClientInfo {
    /// Local vault wallet id, or `gw-<id>` for server-only rows.
    pub id: String,
    pub chain: String,
    pub address: String,
    pub label: Option<String>,
    pub paused: bool,
    pub primary: bool,
    /// Drift between local vault + gateway registry, when detectable.
    pub drift: Option<String>,
    /// The gateway's row. `None` while the endpoint doesn't exist or
    /// auth is missing.
    pub gateway: Option<GatewayClient>,
}

/// Pure merge of the local vault entries with the gateway rows.
/// `gw_rows = None` ⇒ gateway unreachable / endpoint absent (no drift
/// detectable). Wallets the gateway doesn't know get a drift note;
/// server-only rows are appended as `gw-…`.
pub fn merge_clients(
    local: &[WalletEntry],
    primary_sol: Option<&str>,
    primary_hl: Option<&str>,
    gw_rows: Option<&[GatewayClient]>,
) -> Vec<ClientInfo> {
    let mut out = Vec::with_capacity(local.len());
    for entry in local {
        let gw = gw_rows.and_then(|rows| {
            rows.iter()
                .find(|r| {
                    r.wallet
                        .as_deref()
                        .is_some_and(|w| w.eq_ignore_ascii_case(&entry.address))
                })
                .cloned()
        });
        let drift =
            (gw_rows.is_some() && gw.is_none()).then(|| "not registered server-side".to_string());
        out.push(ClientInfo {
            id: entry.id.clone(),
            chain: entry.chain.as_str().to_string(),
            address: entry.address.clone(),
            label: entry.label.clone(),
            paused: entry.paused,
            primary: Some(entry.id.as_str()) == primary_sol
                || Some(entry.id.as_str()) == primary_hl,
            drift,
            gateway: gw,
        });
    }
    if let Some(rows) = gw_rows {
        for r in rows {
            let known = r
                .wallet
                .as_deref()
                .is_some_and(|w| local.iter().any(|e| e.address.eq_ignore_ascii_case(w)));
            if known {
                continue;
            }
            out.push(ClientInfo {
                id: format!("gw-{}", r.id),
                chain: r.chain.clone().unwrap_or_else(|| "?".into()),
                address: r.wallet.clone().unwrap_or_default(),
                label: r.label.clone(),
                paused: r.paused.unwrap_or(false),
                primary: false,
                drift: Some("no local key for this client".into()),
                gateway: Some(r.clone()),
            });
        }
    }
    out
}

/// Resolve a user-supplied client selector against the vault: exact id,
/// unique id prefix, exact address (case-insensitive), or unique label.
pub fn resolve_selector(vault: &Vault, sel: &str) -> Result<WalletEntry, String> {
    let s = sel.trim();
    if let Some(w) = vault.get(s) {
        return Ok(w.clone());
    }
    let by_addr: Vec<&WalletEntry> = vault
        .wallets()
        .iter()
        .filter(|w| w.address.eq_ignore_ascii_case(s))
        .collect();
    if by_addr.len() == 1 {
        return Ok(by_addr[0].clone());
    }
    let by_prefix: Vec<&WalletEntry> = vault
        .wallets()
        .iter()
        .filter(|w| w.id.starts_with(s))
        .collect();
    if by_prefix.len() == 1 {
        return Ok(by_prefix[0].clone());
    }
    let by_label: Vec<&WalletEntry> = vault
        .wallets()
        .iter()
        .filter(|w| {
            w.label
                .as_deref()
                .is_some_and(|l| l.eq_ignore_ascii_case(s))
        })
        .collect();
    if by_label.len() == 1 {
        return Ok(by_label[0].clone());
    }
    if by_prefix.len() > 1 || by_addr.len() > 1 || by_label.len() > 1 {
        return Err(format!("selector {s:?} is ambiguous — use the full id"));
    }
    Err(format!(
        "no client matches {s:?} (id / id-prefix / address / label) — run `clients list`"
    ))
}

/// Parse a pasted Solana secret: base58 or hex, 32-byte seed or 64-byte
/// keypair (seed = first half). Same acceptance as the app.
pub fn parse_sol_secret(secret: &str) -> Result<[u8; 32], String> {
    let s = secret.trim();
    let bytes = if let Ok(b) = bs58::decode(s).into_vec() {
        b
    } else {
        hex::decode(s.trim_start_matches("0x"))
            .map_err(|e| format!("secret must be base58 or hex: {e}"))?
    };
    match bytes.len() {
        32 => Ok(bytes.as_slice().try_into().unwrap()),
        64 => Ok(bytes[..32].try_into().unwrap()),
        n => Err(format!("secret must be 32 or 64 bytes, got {n}")),
    }
}

// ─── core mutations (shared by headless commands + TUI modals) ──────

/// Generate a fresh Solana wallet into the vault (vault-append).
pub fn client_add_sol(label: Option<String>, password: &str) -> Result<WalletEntry, String> {
    let mut vault = open_or_create_vault_migrated(password)?;
    let kp = core::Keypair::new();
    let mut seed = [0u8; 32];
    seed.copy_from_slice(kp.secret_bytes().as_slice());
    drop(kp);
    let entry = vault
        .add_sol(&seed, password, label)
        .map_err(|e| e.to_string());
    {
        use zeroize::Zeroize as _;
        seed.zeroize();
    }
    entry
}

/// Import a pasted private key into the vault (per chain, N times).
pub fn client_import(
    chain: WalletChain,
    secret: &str,
    label: Option<String>,
    password: &str,
) -> Result<WalletEntry, String> {
    let mut vault = open_or_create_vault_migrated(password)?;
    match chain {
        WalletChain::Sol => {
            let mut seed = parse_sol_secret(secret)?;
            let entry = vault
                .add_sol(&seed, password, label)
                .map_err(|e| e.to_string());
            {
                use zeroize::Zeroize as _;
                seed.zeroize();
            }
            entry
        }
        WalletChain::Hl => vault
            .add_hl(secret.trim(), password, label)
            .map_err(|e| e.to_string()),
    }
}

// ─── headless commands ──────────────────────────────────────────────

fn short_addr(a: &str) -> String {
    if a.len() <= 14 {
        a.to_string()
    } else {
        format!("{}…{}", &a[..6], &a[a.len() - 6..])
    }
}

/// `clients list` — vault wallets merged with the gateway registry.
/// Local wallets missing server-side are auto-registered (best-effort,
/// like the app's list) and flagged.
pub async fn cmd_list(json: bool, no_gateway: bool) -> Result<()> {
    let vault = open_vault().map_err(|e| anyhow!(e))?;
    let local: Vec<WalletEntry> = vault
        .as_ref()
        .map(|v| v.wallets().to_vec())
        .unwrap_or_default();
    let primary_sol = vault
        .as_ref()
        .and_then(|v| v.primary(WalletChain::Sol).map(|w| w.id.clone()));
    let primary_hl = vault
        .as_ref()
        .and_then(|v| v.primary(WalletChain::Hl).map(|w| w.id.clone()));

    let auth = if no_gateway { None } else { gateway_auth() };
    let gw_rows = match &auth {
        Some(a) => match fetch_gateway_clients(a).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "gateway clients fetch failed — local-only view");
                None
            }
        },
        None => None,
    };

    let mut infos = merge_clients(
        &local,
        primary_sol.as_deref(),
        primary_hl.as_deref(),
        gw_rows.as_deref(),
    );

    // Drift repair: register local wallets the gateway doesn't know.
    if let (Some(a), Some(_)) = (&auth, &gw_rows) {
        for info in infos.iter_mut() {
            if info.drift.as_deref() == Some("not registered server-side") {
                if let Some(entry) = local.iter().find(|e| e.id == info.id) {
                    info.drift = Some(match register_gateway_client(a, entry).await {
                        Ok(()) => "registered server-side just now".into(),
                        Err(e) => format!("not registered server-side (auto-register failed: {e})"),
                    });
                }
            }
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&infos)?);
        return Ok(());
    }

    println!("{}", branding::wordmark());
    let dir = vault_dir().map_err(|e| anyhow!(e))?;
    if vault.is_none() {
        println!(
            "  {} {}",
            branding::muted("·"),
            branding::muted(&format!(
                "No vault yet at {} — create one with `clients add` or `clients import`.",
                dir.display()
            ))
        );
    } else {
        println!(
            "  {} {}",
            branding::muted("·"),
            branding::muted(&format!(
                "Vault: {} ({} wallet(s))",
                dir.display(),
                local.len()
            ))
        );
    }
    match (&auth, &gw_rows) {
        (None, _) => println!(
            "  {} {}",
            branding::muted("·"),
            branding::muted(
                "Gateway: not connected (run `login` or pair via `register`) — local-only view"
            )
        ),
        (Some(a), None) => println!(
            "  {} {}",
            branding::muted("·"),
            branding::muted(&format!(
                "Gateway: {} — clients endpoint unavailable, local-only view",
                a.base
            ))
        ),
        (Some(a), Some(rows)) => println!(
            "  {} {}",
            branding::muted("·"),
            branding::muted(&format!("Gateway: {} ({} row(s))", a.base, rows.len()))
        ),
    }
    println!();
    if infos.is_empty() {
        println!("  {}", branding::muted("(no clients)"));
        return Ok(());
    }
    println!(
        "  {}",
        branding::muted(&format!(
            "{:1} {:5} {:18} {:17} {:8} {:9} {:12}  {}",
            "", "CHAIN", "LABEL", "ADDRESS", "PRIMARY", "PAUSED", "ID", "GATEWAY / DRIFT"
        ))
    );
    for i in &infos {
        let gw_col = match (&i.gateway, &i.drift) {
            (_, Some(d)) => d.clone(),
            (Some(g), None) => {
                let pos = g
                    .open_positions
                    .map(|p| format!("{p} pos"))
                    .unwrap_or_else(|| "ok".into());
                let act = g
                    .last_activity
                    .as_deref()
                    .map(|a| format!(", last {a}"))
                    .unwrap_or_default();
                format!("registered ({pos}{act})")
            }
            (None, None) => "—".into(),
        };
        let line = format!(
            "{:1} {:5} {:18} {:17} {:8} {:9} {:12}  {}",
            if i.primary { "★" } else { "" },
            i.chain,
            i.label.clone().unwrap_or_else(|| "—".into()),
            short_addr(&i.address),
            if i.primary { "yes" } else { "" },
            if i.paused { "PAUSED" } else { "" },
            &i.id[..i.id.len().min(12)],
            gw_col,
        );
        if i.paused {
            println!("  {}", branding::warn(&line));
        } else {
            println!("  {}", branding::ink(&line));
        }
    }
    println!();
    println!(
        "  {}",
        branding::muted(
            "★ = designated primary for its chain (owns legacy/unstamped work). Secondary HL \
             wallets execute their own wallet-scoped work on multi-client gateways."
        )
    );
    Ok(())
}

/// `clients add` — generate a fresh Solana wallet into the vault.
/// (HL agent keys are minted on hyperliquid.xyz — use `clients import`.)
pub fn cmd_add(chain: String, label: Option<String>, password_stdin: bool) -> Result<()> {
    println!("{}", branding::wordmark());
    if chain != "sol" {
        return Err(anyhow!(
            "only `--chain sol` can be generated locally. HL API agent keys are \
             minted on https://app.hyperliquid.xyz/API — import one with \
             `clients import --chain hl`"
        ));
    }
    let password = read_master_password_for_append(password_stdin)?;
    let entry = client_add_sol(label, &password).map_err(|e| anyhow!(e))?;
    print_added(&entry);
    Ok(())
}

/// `clients import` — paste an existing private key (sol: base58/hex
/// seed or 64-byte keypair; hl: 32-byte hex). The secret is read from
/// a hidden prompt, or the FIRST stdin line with `--secret-stdin`
/// (password then comes from the second line with `--password-stdin`).
pub fn cmd_import(
    chain: String,
    label: Option<String>,
    secret_stdin: bool,
    password_stdin: bool,
) -> Result<()> {
    println!("{}", branding::wordmark());
    let chain = match chain.as_str() {
        "sol" => WalletChain::Sol,
        "hl" => WalletChain::Hl,
        other => return Err(anyhow!("unknown chain {other:?} (sol | hl)")),
    };
    if chain == WalletChain::Hl {
        println!(
            "  {} paste your sandboxed HL {} key — NEVER your main-wallet key.",
            branding::warn("⚠"),
            branding::accent_bold("API agent")
        );
    }
    let secret = if secret_stdin {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        buf.trim().to_string()
    } else {
        rpassword::prompt_password(format!("  {} Private key (hidden): ", branding::prefix()))?
            .trim()
            .to_string()
    };
    if secret.is_empty() {
        return Err(anyhow!("no private key entered"));
    }
    let password = read_master_password_for_append(password_stdin)?;
    let entry = client_import(chain, &secret, label, &password).map_err(|e| anyhow!(e))?;
    print_added(&entry);
    Ok(())
}

fn print_added(entry: &WalletEntry) {
    println!();
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::muted("Added to vault:"),
        branding::accent_bold(&entry.address)
    );
    println!(
        "  {} {} {}",
        branding::muted("·"),
        branding::muted("Client id:     "),
        branding::ink(&entry.id)
    );
    if entry.chain == WalletChain::Hl {
        println!(
            "  {} {}",
            branding::muted("·"),
            branding::muted(
                "Pair it with the gateway next: `hl-signer-desktop register --token=… --account=0x…`"
            )
        );
    }
    println!(
        "  {} {}",
        branding::muted("·"),
        branding::muted("Runtimes pick it up on the next `run` / TUI unlock.")
    );
}

/// `clients remove` — drop a wallet from the vault (keystore kept as
/// `<file>.removed.bak`, never destroyed).
pub fn cmd_remove(selector: String, yes: bool) -> Result<()> {
    println!("{}", branding::wordmark());
    let mut vault = open_vault()
        .map_err(|e| anyhow!(e))?
        .ok_or_else(|| anyhow!("no vault on this device"))?;
    let entry = resolve_selector(&vault, &selector).map_err(|e| anyhow!(e))?;
    if !yes {
        print!(
            "  {} Remove {} ({})? The encrypted keystore is kept as .removed.bak. [y/N]: ",
            branding::prefix(),
            branding::accent_bold(&entry.address),
            entry.chain.as_str(),
        );
        use std::io::Write as _;
        std::io::stdout().flush()?;
        let mut a = String::new();
        std::io::stdin().read_line(&mut a)?;
        if !matches!(a.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("  Aborted.");
            return Ok(());
        }
    }
    let removed = vault.remove(&entry.id).map_err(|e| anyhow!("{e}"))?;
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::muted("Removed from vault:"),
        branding::accent_bold(&removed.address)
    );
    println!(
        "  {} {}",
        branding::muted("·"),
        branding::muted(&format!(
            "Keystore preserved as {}.removed.bak",
            removed.file
        ))
    );
    Ok(())
}

/// `clients label` — set / clear a wallet's label.
pub fn cmd_label(selector: String, label: Option<String>) -> Result<()> {
    let mut vault = open_vault()
        .map_err(|e| anyhow!(e))?
        .ok_or_else(|| anyhow!("no vault on this device"))?;
    let entry = resolve_selector(&vault, &selector).map_err(|e| anyhow!(e))?;
    vault
        .set_label(&entry.id, label.clone())
        .map_err(|e| anyhow!("{e}"))?;
    println!("{}", branding::wordmark());
    println!(
        "  {} {} {} {}",
        branding::tick(),
        branding::accent_bold(&short_addr(&entry.address)),
        branding::muted("label →"),
        branding::ink(label.as_deref().unwrap_or("(cleared)"))
    );
    Ok(())
}

/// `clients pause` / `clients resume` — flip the per-client kill-switch
/// in the vault (the running app/`run` daemon reads it on its next
/// unlock; a LIVE `run` process applies pause at startup — restart to
/// apply) and push the toggle to the gateway best-effort.
pub async fn cmd_pause(selector: String, paused: bool) -> Result<()> {
    let mut vault = open_vault()
        .map_err(|e| anyhow!(e))?
        .ok_or_else(|| anyhow!("no vault on this device"))?;
    let entry = resolve_selector(&vault, &selector).map_err(|e| anyhow!(e))?;
    vault
        .set_paused(&entry.id, paused)
        .map_err(|e| anyhow!("{e}"))?;
    push_pause_best_effort(entry.address.clone(), paused).await;
    println!("{}", branding::wordmark());
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::accent_bold(&short_addr(&entry.address)),
        if paused {
            branding::warn("PAUSED (persisted; gateway notified best-effort)")
        } else {
            branding::ink("resumed (persisted; gateway notified best-effort)")
        }
    );
    println!(
        "  {} {}",
        branding::muted("·"),
        branding::muted(
            "A running `run` daemon / TUI applies this on its next start; \
             the desktop app applies it live."
        )
    );
    Ok(())
}

/// `clients set-primary` — designate a wallet as its chain's executor.
pub fn cmd_set_primary(selector: String) -> Result<()> {
    let mut vault = open_vault()
        .map_err(|e| anyhow!(e))?
        .ok_or_else(|| anyhow!("no vault on this device"))?;
    let entry = resolve_selector(&vault, &selector).map_err(|e| anyhow!(e))?;
    vault.set_primary(&entry.id).map_err(|e| anyhow!("{e}"))?;
    println!("{}", branding::wordmark());
    println!(
        "  {} {} {} {}",
        branding::tick(),
        branding::accent_bold(&short_addr(&entry.address)),
        branding::muted("is now the"),
        branding::accent_bold(&format!("{} primary", entry.chain.as_str()))
    );
    println!(
        "  {} {}",
        branding::muted("·"),
        branding::muted("Restart `run` / re-unlock the TUI for the executor swap to take effect.")
    );
    Ok(())
}

/// Claim scope for a vault PRIMARY HL daemon: wallet-scoped on its
/// paired master account (`?wallet=` + the core per-row belt), with
/// `allow_unstamped` so legacy rows from a pre-multi-client gateway
/// keep executing exactly once on the wallet that always owned them.
/// Unpaired (`account_address` unset) falls back to the legacy
/// unscoped claim — identical to the single-wallet daemon.
pub fn hl_primary_claim_scope(cfg: &config::Config) -> crate::daemon::ClaimScope {
    match cfg.account_address.as_deref() {
        Some(master) if !master.is_empty() => crate::daemon::ClaimScope::Scoped {
            wallet: master.to_string(),
            allow_unstamped: true,
        },
        _ => crate::daemon::ClaimScope::Unscoped,
    }
}

// ─── HL secondary executor (probe → scoped daemon | standby) ───────

/// Does this gateway scope the HL claim queue per wallet?
///
/// There is no direct capability endpoint; `GET /api/trading/clients`
/// shipped in the SAME backend slice as the `?wallet=` claim filter +
/// `target_wallet` stamping, so its existence is the deploy proxy
/// (same probe the desktop app runs): `Ok(true)` = wallet scoping
/// live, `Ok(false)` = 404/405 (older gateway), `Err` = undetermined
/// (network/auth) — the caller retries, then falls back to standby.
/// This gate matters because an old gateway silently IGNORES the
/// unknown `?wallet=` param: a secondary daemon polling it would claim
/// other wallets' instructions.
async fn gateway_supports_wallet_scoping(cfg: &config::Config) -> Result<bool, String> {
    let token = cfg.api_token.clone().ok_or("not paired")?;
    let url = format!(
        "{}/api/trading/clients",
        cfg.server_url.trim_end_matches('/')
    );
    let resp = http()
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| format!("capability probe: {e}"))?;
    let status = resp.status();
    if status.as_u16() == 404 || status.as_u16() == 405 {
        return Ok(false);
    }
    if status.is_success() {
        return Ok(true);
    }
    Err(format!("capability probe: gateway {status}"))
}

/// Everything a NON-PRIMARY HL wallet's task needs.
pub struct HlSecondaryArgs {
    pub entry: WalletEntry,
    pub cfg: config::Config,
    pub secret_hex: String,
    pub hl_runtime: core::hl::runtime::SharedHlRuntime,
    /// TUI telemetry mirror (orders ring). `None` headless.
    pub tui_runtime: Option<crate::daemon::SharedRuntime>,
    pub pause: crate::daemon::SharedPause,
    pub executed_path: PathBuf,
    pub poll_secs: Option<u64>,
    pub nats_url: Option<String>,
}

/// Spawn the runtime for a NON-PRIMARY HL wallet — the app's
/// `spawn_hl_secondary` topology:
///
/// Paired + multi-client gateway → a FULL poll/sign/report daemon with
/// a STRICT per-wallet claim scope (`?wallet=master`, unstamped rows
/// refused — those belong to the primary), its own executed ledger and
/// pause gate. Unpaired, or the gateway predates wallet scoping → the
/// register-heartbeat + balance standby loop (never claims).
pub fn spawn_hl_secondary(args: HlSecondaryArgs) -> tokio::task::JoinHandle<Result<()>> {
    use core::hl::runtime::ConnState;
    use std::sync::atomic::Ordering;

    let HlSecondaryArgs {
        entry,
        cfg,
        secret_hex,
        hl_runtime: runtime,
        tui_runtime,
        pause,
        executed_path,
        poll_secs,
        nats_url,
    } = args;
    runtime.daemon_running.store(true, Ordering::SeqCst);
    let my_generation = runtime.run_generation.fetch_add(1, Ordering::SeqCst) + 1;
    tokio::spawn(async move {
        let paired = cfg.api_token.is_some() && cfg.agent_address.is_some();
        let master = cfg.account_address.clone();
        let mut run_full_daemon = false;
        if paired && master.is_some() {
            // Probe (bounded retries on undetermined) — a restart
            // re-probes.
            for attempt in 0..3u8 {
                if !runtime.daemon_running.load(Ordering::Relaxed)
                    || runtime.run_generation.load(Ordering::SeqCst) != my_generation
                {
                    runtime.set_conn(ConnState::Offline);
                    return Ok(());
                }
                match gateway_supports_wallet_scoping(&cfg).await {
                    Ok(supported) => {
                        run_full_daemon = supported;
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(wallet = %entry.address, error = %e,
                            "wallet-scoping capability probe failed — retrying");
                        if attempt == 2 {
                            runtime.set_error(Some(format!(
                                "could not verify gateway wallet scoping ({e}) — running standby; restart to retry"
                            )));
                        } else {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    }
                }
            }
        }

        if run_full_daemon {
            let master = master.expect("checked above");
            tracing::info!(wallet = %entry.address, %master,
                "secondary HL wallet: gateway scopes claims per wallet — starting full executor");
            let poll = poll_secs.unwrap_or(cfg.poll_secs).max(1);
            let opts = crate::daemon::DaemonOpts {
                config: cfg,
                secret_hex,
                agent_address: entry.address.clone(),
                poll_interval: std::time::Duration::from_secs(poll),
                nats_url,
                pause: Some(pause),
                runtime: tui_runtime,
                hl_runtime: runtime.clone(),
                paper_mode: std::env::var("HL_SIGNER_PAPER")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false),
                config_dir: None,
                executed_path: Some(executed_path),
                // STRICT scope: unstamped rows are the primary's.
                claim_scope: crate::daemon::ClaimScope::Scoped {
                    wallet: master,
                    allow_unstamped: false,
                },
                // Secondary daemons never own the terminal: no banner,
                // no stdin TOTP prompt (the core's bounded wait acks
                // `failed` if a challenge ever fires — per-trade TOTP
                // is off in prod).
                banner: false,
                stdin_totp: false,
            };
            let r = crate::daemon::run(opts).await;
            if let Err(e) = &r {
                tracing::error!(error = %e, wallet = %entry.address,
                    "secondary HL daemon exited with error");
                runtime.set_conn(ConnState::Error);
                runtime.set_error(Some(format!("daemon stopped: {e}")));
            }
            r
        } else {
            hl_standby_loop(&entry, &cfg, &runtime, my_generation).await;
            Ok(())
        }
    })
}

/// Standby loop for an HL wallet that cannot execute (unpaired, or the
/// gateway predates per-wallet claim scoping): keep the gateway pairing
/// heartbeat alive (when paired) + refresh the master-account balance
/// into this wallet's own telemetry. NEVER polls `instructions/pending`.
async fn hl_standby_loop(
    entry: &WalletEntry,
    cfg: &config::Config,
    runtime: &core::hl::runtime::SharedHlRuntime,
    my_generation: u64,
) {
    use core::hl::config::NetworkChoice;
    use core::hl::info::HttpInfoClient;
    use core::hl::runtime::ConnState;
    use core::hl::server::{RegisterReq, ServerClient};
    use platform_hl_exchange::Network;
    use std::sync::atomic::Ordering;

    {
        let network = match cfg.network {
            NetworkChoice::Mainnet => Network::Mainnet,
            NetworkChoice::Testnet => Network::Testnet,
        };
        let server = cfg
            .api_token
            .clone()
            .and_then(|t| ServerClient::new(cfg.server_url.clone(), t).ok());
        let info = HttpInfoClient::new(network).ok();
        if server.is_none() {
            runtime.set_error(Some(
                "standby — not paired yet (no instruction polling; pair via `register` to execute)"
                    .into(),
            ));
        } else {
            runtime.set_error(None);
        }
        runtime.set_conn(ConnState::Connecting);
        if let Ok(mut g) = runtime.account_address.lock() {
            *g = cfg.account_address.clone();
        }
        if let Ok(mut g) = runtime.agent_address.lock() {
            *g = Some(entry.address.clone());
        }
        let mut tick: u64 = 0;
        loop {
            if !runtime.daemon_running.load(Ordering::Relaxed)
                || runtime.run_generation.load(Ordering::SeqCst) != my_generation
            {
                runtime.set_conn(ConnState::Offline);
                return;
            }
            // Heartbeat register every 60 s (tick 0, 6, 12, …).
            if tick % 6 == 0 {
                if let Some(server) = &server {
                    let req = RegisterReq {
                        agent_address: entry.address.clone(),
                        client_version: Some(format!(
                            "hl-signer-desktop {} (standby)",
                            env!("CARGO_PKG_VERSION")
                        )),
                        host_id: cfg.host_id.clone(),
                        paired_with_account: cfg.account_address.clone(),
                    };
                    match server.register(&req).await {
                        Ok(_) => {
                            runtime.set_conn(ConnState::Ready);
                            runtime.set_error(None);
                        }
                        Err(e) => {
                            runtime.set_conn(ConnState::Error);
                            runtime.set_error(Some(format!("standby register: {e}")));
                        }
                    }
                }
            }
            // Balance refresh every tick (10 s) off the master account.
            if let (Some(info), Some(master)) = (&info, cfg.account_address.as_deref()) {
                match info.account_summary(master).await {
                    Ok(summary) => {
                        let positions = summary
                            .positions
                            .iter()
                            .map(|p| core::hl::runtime::PositionRow {
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
                            *g = core::hl::runtime::BalanceSnapshot {
                                account_value_usd: summary.account_value_usd,
                                withdrawable_usd: summary.withdrawable_usd,
                                positions,
                                fetched_at: Some(chrono::Utc::now()),
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
            tick = tick.wrapping_add(1);
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    }
}

// ─── `run` — the headless multi-client runtime ──────────────────────

pub struct RunArgs {
    pub password_stdin: bool,
    /// Poll cadence override for the HL primary daemon (else its config).
    pub poll_secs: Option<u64>,
    /// Optional NATS push URL for the HL primary daemon.
    pub nats_url: Option<String>,
    /// `:5829` web-bridge port.
    pub port: u16,
    /// Explicit gateway JWT file (else desktop-auth / pairing JWT / web push).
    pub token_file: Option<PathBuf>,
    /// Copy-session budget override (SOL).
    pub session_sol: Option<f64>,
}

/// The headless equivalent of the unlocked desktop app: unlock ALL
/// vault wallets with the master password and bring the fleet online —
/// HL primary = full daemon, HL standbys = heartbeat+balance, Sol
/// primary = sell/copy engine + `:5829` bridge, Sol standbys idle.
pub async fn cmd_run(args: RunArgs) -> Result<()> {
    eprintln!("{}", branding::wordmark());
    let dir = vault_dir().map_err(|e| anyhow!(e))?;
    if !Vault::exists(&dir) {
        return Err(anyhow!(
            "no vault at {} — add a wallet first (`clients add` / `clients import`), \
             or keep using the single-wallet `daemon` / `sol daemon` commands",
            dir.display()
        ));
    }
    let password = read_master_password(args.password_stdin)?;
    // Open + adopt any legacy keystores (idempotent), exactly like the
    // app's unlock.
    let vault = open_or_create_vault_migrated(&password).map_err(|e| anyhow!(e))?;

    let primary_sol = vault.primary(WalletChain::Sol).map(|w| w.id.clone());
    let primary_hl = vault.primary(WalletChain::Hl).map(|w| w.id.clone());
    let mut hl_count = (0usize, 0usize); // (executors, standbys)
    let mut handles: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();

    // ── HL wallets ──
    for entry in vault.wallets().iter().cloned() {
        if entry.chain != WalletChain::Hl {
            continue;
        }
        let is_primary = Some(&entry.id) == primary_hl.as_ref();
        let cfg = hl_config_for(&vault, &entry, is_primary);
        let (secret_hex, address) = vault
            .unlock_hl(&entry.id, &password)
            .map_err(|e| anyhow!("unlock {} ({}): {e}", entry.address, entry.id))?;
        let display = entry
            .label
            .clone()
            .unwrap_or_else(|| short_addr(&entry.address));
        if is_primary {
            if cfg.api_token.is_none() || cfg.agent_address.is_none() {
                eprintln!(
                    "  {} HL primary {} is not paired — run `hl-signer-desktop register` \
                     first. Skipping its executor.",
                    branding::warn("!"),
                    branding::accent_bold(&display)
                );
                continue;
            }
            let executed = hl_executed_path_seeded(&vault, &entry);
            let poll = args.poll_secs.unwrap_or(cfg.poll_secs).max(1);
            let claim_scope = hl_primary_claim_scope(&cfg);
            let opts = crate::daemon::DaemonOpts {
                config: cfg,
                secret_hex,
                agent_address: address,
                poll_interval: std::time::Duration::from_secs(poll),
                nats_url: args.nats_url.clone(),
                pause: Some(Arc::new(Mutex::new(entry.paused))),
                runtime: None,
                hl_runtime: crate::daemon::fresh_hl_runtime(),
                paper_mode: std::env::var("HL_SIGNER_PAPER")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false),
                config_dir: None,
                executed_path: Some(executed),
                claim_scope,
                banner: true,
                stdin_totp: true,
            };
            if entry.paused {
                eprintln!(
                    "  {} HL primary {} starts {} (vault per-client pause; \
                     `clients resume` + restart to trade)",
                    branding::warn("!"),
                    branding::accent_bold(&display),
                    branding::warn("PAUSED")
                );
            }
            eprintln!(
                "  {} {}  {} {}",
                branding::brand_tag(),
                branding::status_pill("hl"),
                branding::muted("primary executor"),
                branding::accent_bold(&display)
            );
            handles.push(tokio::spawn(async move {
                let r = crate::daemon::run(opts).await;
                if let Err(e) = &r {
                    tracing::error!(error = %e, "HL primary daemon stopped");
                }
                r
            }));
            hl_count.0 += 1;
        } else {
            eprintln!(
                "  {} {}  {} {}",
                branding::brand_tag(),
                branding::status_pill("hl"),
                branding::muted(
                    "secondary (wallet-scoped executor when the gateway supports it; \
                     heartbeat standby otherwise)"
                ),
                branding::accent_bold(&display)
            );
            let executed = hl_executed_path_seeded(&vault, &entry);
            handles.push(spawn_hl_secondary(HlSecondaryArgs {
                entry: entry.clone(),
                cfg,
                secret_hex,
                hl_runtime: crate::daemon::fresh_hl_runtime(),
                tui_runtime: None,
                pause: Arc::new(Mutex::new(entry.paused)),
                executed_path: executed,
                poll_secs: args.poll_secs,
                nats_url: args.nats_url.clone(),
            }));
            hl_count.1 += 1;
        }
    }

    // ── Sol primary ──
    let mut serve_task: Option<tokio::task::JoinHandle<Result<(), anyhow::Error>>> = None;
    if let Some(pid) = &primary_sol {
        let entry = vault
            .get(pid)
            .cloned()
            .expect("primary id resolves to a vault entry");
        let kp = vault
            .unlock_sol(pid, &password)
            .map_err(|e| anyhow!("unlock {} ({pid}): {e}", entry.address))?;
        let display = entry
            .label
            .clone()
            .unwrap_or_else(|| short_addr(&entry.address));
        let standby_sols = vault
            .wallets()
            .iter()
            .filter(|w| w.chain == WalletChain::Sol && &w.id != pid)
            .count();

        let sol_cfg = {
            let mut c = crate::sol::config::SolConfig::load_or_default();
            if let Some(s) = args.session_sol {
                c.copy_session_sol = Some(s);
            }
            c
        };

        // `:5829` bridge — armed with the primary keypair.
        let slot = core::SignerSlot::default();
        let runtime_kp = core::Keypair::try_from(&kp.to_bytes()[..])
            .map_err(|e| anyhow!("keypair clone: {e}"))?;
        slot.install(kp);
        let gateway_base = config::Config::load_or_default().server_url;
        let daemon_state =
            core::LocalDaemonState::new(slot, gateway_base.clone(), sol_cfg.resolved_rpc_url())
                .with_client_kind("hl-signer-desktop");
        let web_config = daemon_state.config.clone();
        // Seed the bridge with the Discord-minted JWT (app parity) so
        // web-app swaps relay without a manual /setAuth push.
        if let Some(a) = crate::auth::DesktopAuth::load_valid() {
            web_config.write().await.auth_token = Some(a.token);
        }
        let port = args.port;
        serve_task = Some(tokio::spawn(async move {
            core::serve_local_daemon(daemon_state, port).await
        }));

        eprintln!(
            "  {} {}  {} {}{}",
            branding::brand_tag(),
            branding::status_pill("sol"),
            branding::muted("primary executor"),
            branding::accent_bold(&display),
            if standby_sols > 0 {
                branding::muted(&format!("  (+{standby_sols} standby — idle by design)"))
            } else {
                String::new()
            }
        );

        if entry.paused {
            eprintln!(
                "  {} Sol primary {} is {} — engine not started \
                 (`clients resume` + restart). The :{port} bridge still serves.",
                branding::warn("!"),
                branding::accent_bold(&display),
                branding::warn("PAUSED"),
            );
        } else {
            if sol_cfg.copy_session_sol.is_none() {
                eprintln!(
                    "  {} copy buys DISARMED — no session budget. Set one with \
                     `sol budget --session-sol <SOL>` or pass --session-sol. \
                     TP/SL + mirror sells still execute.",
                    branding::warn("!")
                );
            }
            let auth = match &args.token_file {
                Some(tf) => {
                    let token = std::fs::read_to_string(tf)
                        .context("read token file")?
                        .trim()
                        .to_string();
                    if token.is_empty() {
                        return Err(anyhow!("token file is empty"));
                    }
                    crate::sol::runtime::AuthSource::Token {
                        base: gateway_base.trim_end_matches('/').to_string(),
                        token,
                    }
                }
                None => crate::sol::runtime::AuthSource::Auto {
                    web: Some(web_config),
                },
            };
            let rt: crate::sol::runtime::SharedSolRuntime = Arc::default();
            crate::sol::runtime::spawn(
                rt,
                crate::sol::runtime::SpawnArgs {
                    kp: Arc::new(runtime_kp),
                    auth,
                    cfg: sol_cfg,
                    stdout_log: true,
                },
            );
        }
    } else {
        eprintln!(
            "  {} {}",
            branding::muted("·"),
            branding::muted("no Solana wallet in the vault — Sol engine + :5829 bridge skipped")
        );
    }

    eprintln!(
        "  {} {}  {}",
        branding::brand_tag(),
        branding::status_pill("fleet"),
        branding::muted(&format!(
            "{} HL primary, {} HL secondary wallet(s) live — Ctrl-C to stop",
            hl_count.0, hl_count.1
        ))
    );

    // Block: on the :5829 bridge when serving (a bind failure is a
    // clean, actionable error), else until Ctrl-C.
    match serve_task {
        Some(t) => match t.await {
            Ok(r) => r.map_err(|e| anyhow!("{e}")),
            Err(e) => Err(anyhow!("local daemon task panicked: {e}")),
        },
        None => {
            tokio::signal::ctrl_c().await.ok();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, chain: WalletChain, address: &str, paused: bool) -> WalletEntry {
        WalletEntry {
            id: id.into(),
            chain,
            address: address.into(),
            label: None,
            created_at: chrono::Utc::now(),
            file: format!("{}-{address}.json", chain.as_str()),
            paused,
        }
    }

    fn gw(id: &str, wallet: &str) -> GatewayClient {
        GatewayClient {
            id: id.into(),
            chain: Some("solana".into()),
            wallet: Some(wallet.into()),
            label: None,
            paused: Some(false),
            budget: None,
            active_config_count: None,
            assignment_count: None,
            open_positions: Some(2),
            unrealized_pnl: None,
            last_activity: None,
        }
    }

    #[test]
    fn merge_flags_primary_drift_and_server_only_rows() {
        let local = vec![
            entry("a", WalletChain::Sol, "So1AAA", false),
            entry("b", WalletChain::Sol, "So1BBB", true),
            entry("c", WalletChain::Hl, "0xCCC", false),
        ];
        let rows = vec![gw("g1", "so1aaa"), gw("g9", "So1ZZZ")];
        let out = merge_clients(&local, Some("a"), Some("c"), Some(&rows));
        assert_eq!(out.len(), 4);
        // Address match is case-insensitive; matched row carries gateway.
        assert!(out[0].primary && out[0].gateway.is_some() && out[0].drift.is_none());
        // Unknown server-side → drift.
        assert_eq!(out[1].drift.as_deref(), Some("not registered server-side"));
        assert!(out[1].paused);
        // HL primary flagged independently of the Sol primary.
        assert!(out[2].primary);
        // Server-only row appended as gw-….
        assert_eq!(out[3].id, "gw-g9");
        assert_eq!(
            out[3].drift.as_deref(),
            Some("no local key for this client")
        );
    }

    #[test]
    fn merge_without_gateway_never_reports_drift() {
        let local = vec![entry("a", WalletChain::Sol, "So1AAA", false)];
        let out = merge_clients(&local, Some("a"), None, None);
        assert_eq!(out.len(), 1);
        assert!(out[0].drift.is_none() && out[0].gateway.is_none());
    }

    #[test]
    fn sol_secret_accepts_base58_hex_and_64_byte_keypairs() {
        let seed = [7u8; 32];
        let b58 = bs58::encode(&seed).into_string();
        assert_eq!(parse_sol_secret(&b58).unwrap(), seed);
        let hexs = hex::encode(seed);
        assert_eq!(parse_sol_secret(&hexs).unwrap(), seed);
        assert_eq!(parse_sol_secret(&format!("0x{hexs}")).unwrap(), seed);
        let mut full = [0u8; 64];
        full[..32].copy_from_slice(&seed);
        let b58full = bs58::encode(&full).into_string();
        assert_eq!(parse_sol_secret(&b58full).unwrap(), seed);
        assert!(parse_sol_secret("abc").is_err());
    }

    #[test]
    fn selector_resolves_id_prefix_address_and_label() {
        let dir = tempfile::tempdir().unwrap();
        let mut v = Vault::create(dir.path(), "hunter22").unwrap();
        let kp = core::Keypair::new();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(kp.secret_bytes().as_slice());
        let e = v
            .add_sol(&seed, "hunter22", Some("trader one".into()))
            .unwrap();
        assert_eq!(resolve_selector(&v, &e.id).unwrap().id, e.id);
        assert_eq!(resolve_selector(&v, &e.id[..8]).unwrap().id, e.id);
        assert_eq!(
            resolve_selector(&v, &e.address.to_uppercase())
                .ok()
                .map(|w| w.id),
            // Sol addresses are case-sensitive base58 — uppercase only
            // matches when it happens to be byte-equal ignoring case;
            // the exact address always resolves.
            resolve_selector(&v, &e.address).ok().map(|w| w.id)
        );
        assert_eq!(resolve_selector(&v, "TRADER ONE").unwrap().id, e.id);
        assert!(resolve_selector(&v, "nope").is_err());
    }

    #[test]
    fn vault_roundtrip_uses_the_apps_on_disk_layout() {
        // Interop pin: the files this CLI writes are exactly the app's
        // vault layout (manifest + per-wallet keystores in one dir).
        let dir = tempfile::tempdir().unwrap();
        let mut v = Vault::create(dir.path(), "hunter22").unwrap();
        let kp = core::Keypair::new();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(kp.secret_bytes().as_slice());
        let s = v.add_sol(&seed, "hunter22", None).unwrap();
        let h = v
            .add_hl(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "hunter22",
                None,
            )
            .unwrap();
        assert!(dir.path().join("vault.json").is_file());
        assert!(dir.path().join(format!("sol-{}.json", s.address)).is_file());
        assert!(dir.path().join(format!("hl-{}.json", h.address)).is_file());
        // Re-open (as the app would) and unlock both under the ONE password.
        let v2 = Vault::open(dir.path()).unwrap();
        assert_eq!(v2.wallets().len(), 2);
        v2.unlock_sol(&s.id, "hunter22").unwrap();
        v2.unlock_hl(&h.id, "hunter22").unwrap();
    }
}
