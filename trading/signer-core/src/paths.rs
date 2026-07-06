//! Cross-platform default paths for the signer family.
//!
//! - Unix: `$XDG_CONFIG_HOME/degenbox` or `~/.config/degenbox`
//! - macOS: same — the Tauri app uses `~/Library/Application Support`
//!   for its own state but keystores live next to the CLI so the user
//!   can flip between Tauri + CLI without re-importing.
//! - Windows: `%APPDATA%\degenbox`
//!
//! Filenames are pinned so a user who installs the desktop app and
//! later runs the CLI sees the same encrypted keystore.

use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathsError {
    #[error("HOME not set on this platform")]
    NoHome,
}

/// The on-disk directory where all DegenBox signer state lives.
pub fn default_dir() -> Result<PathBuf, PathsError> {
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return Ok(PathBuf::from(appdata).join("degenbox"));
        }
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("degenbox"));
    }
    let home = std::env::var("HOME").map_err(|_| PathsError::NoHome)?;
    Ok(PathBuf::from(home).join(".config").join("degenbox"))
}

/// HL agent keystore — encrypted secp256k1 key.
pub fn hl_keystore_path() -> Result<PathBuf, PathsError> {
    Ok(default_dir()?.join("hl-keystore.json"))
}

/// HL signer config — server URL, API token, agent address.
pub fn hl_config_path() -> Result<PathBuf, PathsError> {
    Ok(default_dir()?.join("hl-config.json"))
}

/// Solana hot-wallet keystore — encrypted ed25519 key.
pub fn sol_keystore_path() -> Result<PathBuf, PathsError> {
    Ok(default_dir()?.join("sol-keystore.json"))
}

/// Tauri-app log file. Single file, rotated by size by the app side.
pub fn app_log_path() -> Result<PathBuf, PathsError> {
    Ok(default_dir()?.join("signer-app.log"))
}
