//! On-disk config for `hl-signer-desktop`.
//!
//! Default location: `~/.config/degenbox/hl-config.json`. Holds the
//! server base URL, the agent address (so the daemon can warn if the
//! keystore unlock returns a different one), and the API token used
//! to authenticate to the DegenBox server.
//!
//! The private key itself NEVER lives here — it's in the encrypted
//! keystore file (default `~/.config/degenbox/hl-keystore.json`).

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HOME not set")]
    NoHome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server_url: String,
    /// API token for the DegenBox server. Issued via the dashboard
    /// (Account → API tokens). Sent as `Authorization: Bearer ...`.
    pub api_token: Option<String>,
    pub agent_address: Option<String>,
    /// User's HL master account (the 0x… wallet the agent acts on
    /// behalf of). Required by the `closePosition` and `placeTpsl`
    /// handlers — they need to query HL `/info` for the live position
    /// size + direction. Stored separately from `agent_address`
    /// because they are *different* keypairs: the agent only ever
    /// signs trades; positions are owned by the master.
    #[serde(default)]
    pub account_address: Option<String>,
    pub network: NetworkChoice,
    /// Optional hostname/host id printed in /signer/register so support
    /// can identify which machine the request came from.
    pub host_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkChoice {
    Mainnet,
    Testnet,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: "https://app.degenbox.io".into(),
            api_token: None,
            agent_address: None,
            account_address: None,
            network: NetworkChoice::Mainnet,
            host_id: None,
        }
    }
}

/// `~/.config/degenbox` (or `XDG_CONFIG_HOME/degenbox`).
pub fn default_dir() -> Result<PathBuf, ConfigError> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg).join("degenbox"));
    }
    let home = std::env::var("HOME").map_err(|_| ConfigError::NoHome)?;
    Ok(PathBuf::from(home).join(".config").join("degenbox"))
}

pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    Ok(default_dir()?.join("hl-config.json"))
}

pub fn default_keystore_path() -> Result<PathBuf, ConfigError> {
    Ok(default_dir()?.join("hl-keystore.json"))
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let bytes = fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn save(path: &Path, cfg: &Config) -> Result<(), ConfigError> {
    let bytes = serde_json::to_vec_pretty(cfg)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or_else(|| Path::new(".")))?;
    tmp.write_all(&bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(tmp.path(), perm)?;
    }
    tmp.persist(path).map_err(|e| ConfigError::Io(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn config_roundtrip_preserves_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        let original = Config {
            server_url: "https://staging.example.com".into(),
            api_token: Some("tok_abc".into()),
            agent_address: Some("0xdead".into()),
            account_address: Some("0xbeef".into()),
            network: NetworkChoice::Testnet,
            host_id: Some("alice-mac".into()),
        };
        save(&path, &original).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.server_url, original.server_url);
        assert_eq!(loaded.api_token, original.api_token);
        assert_eq!(loaded.agent_address, original.agent_address);
        assert_eq!(loaded.account_address, original.account_address);
        assert_eq!(loaded.network, original.network);
        assert_eq!(loaded.host_id, original.host_id);
    }

    /// Old configs written before `account_address` existed must still
    /// load — we use `#[serde(default)]` to absorb the missing field.
    #[test]
    fn legacy_config_without_account_address_still_loads() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        // Hand-written legacy shape (no account_address key).
        let legacy = serde_json::json!({
            "server_url": "https://x",
            "api_token": "tok",
            "agent_address": "0xdead",
            "network": "mainnet",
            "host_id": null,
        });
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let loaded = load(&path).unwrap();
        assert!(loaded.account_address.is_none());
        assert_eq!(loaded.agent_address.as_deref(), Some("0xdead"));
    }
}
