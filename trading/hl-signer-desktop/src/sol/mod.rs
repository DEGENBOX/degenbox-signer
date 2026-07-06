//! Solana side of the unified CLI — mirrors the Tauri desktop app's
//! composition on top of `signer-core`:
//!
//! - [`config`]   — `sol-config.json` (shared with the app): mandatory
//!   copy-session budget, slippage/tip/submit-mode, RPC override.
//! - [`runtime`]  — the sell-stream + copy-stream executor (one
//!   `BotEngine`, allowlist → simulate → sign → relay; budget guard on
//!   copy buys).
//! - [`commands`] — headless subcommands (`sol init/import/pubkey/
//!   budget/daemon`).
//! - [`tui`]      — the Solana tab's panel state for the interactive TUI.
//!
//! Keystore: the shared `~/.config/degenbox/sol-keystore.json` (same
//! file the Tauri app and a future CLI build read), with one-shot
//! detection of the legacy `signer-cli` location (`~/.degenbox/
//! keystore.json`) and extension-JSON import via
//! `signer-core::import_extension_json`.

pub mod commands;
pub mod config;
pub mod runtime;
pub mod tui;

use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// Shared keystore path (`~/.config/degenbox/sol-keystore.json`).
pub fn default_keystore_path() -> Result<PathBuf> {
    degenbox_signer_core::sol_keystore_path().map_err(|e| anyhow!("{e}"))
}

/// The legacy `signer-cli` keystore location, when present — offered as
/// a fallback so existing Solana CLI users don't re-import.
pub fn legacy_cli_keystore_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let p = home.join(".degenbox").join("keystore.json");
    p.exists().then_some(p)
}

/// Resolve the keystore to use: explicit flag > shared path > the
/// vault primary's keystore (read-through after the app/CLI migrated
/// the legacy file into the shared multi-wallet vault — same encrypted
/// envelope) > legacy signer-cli path. Errors with a setup hint when
/// none exists.
pub fn resolve_keystore_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let shared = default_keystore_path()?;
    if shared.exists() {
        return Ok(shared);
    }
    if let Some(p) = crate::clients::vault_primary_sol_keystore() {
        tracing::info!(path = %p.display(), "using the vault primary Solana keystore");
        return Ok(p);
    }
    if let Some(legacy) = legacy_cli_keystore_path() {
        tracing::info!(path = %legacy.display(), "using legacy signer-cli Solana keystore");
        return Ok(legacy);
    }
    Err(anyhow!(
        "no Solana keystore found at {} — run `hl-signer-desktop sol init` (generate), \
         `hl-signer-desktop sol import` (existing keystore / extension export), or \
         `hl-signer-desktop clients add` (multi-wallet vault)",
        shared.display()
    ))
}

/// Read the (plaintext) pubkey out of a keystore file without
/// decrypting — it's stored unencrypted for UI convenience.
pub fn peek_pubkey(path: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let ks: degenbox_signer_core::Keystore = serde_json::from_slice(&bytes).ok()?;
    Some(ks.pubkey)
}
