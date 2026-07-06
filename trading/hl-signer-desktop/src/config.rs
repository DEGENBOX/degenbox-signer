//! On-disk config for `hl-signer-desktop`.
//!
//! The config STRUCT is the shared one from `signer-core`
//! (`hl::config::HlConfig`, aliased to `Config` here) so the CLI, the
//! Tauri app and any future front-end round-trip ONE `hl-config.json`
//! shape — including the CLI-owned `client_name` (typed in core since
//! the unification) and forward-compatible keys via the flattened
//! `extra` map.
//!
//! What stays CLI-side in this module is the host glue the shared
//! crate deliberately does not own:
//!
//! - arbitrary-path `load` / `save` (core only reads the fixed shared
//!   path; the multi-client hub points each bot at its own dir),
//! - the `bots/` directory layout + `discover_bots` auto-migration,
//! - the legacy path helpers (`default_*_path`) every subcommand uses.
//!
//! The private key itself NEVER lives here — it's in the encrypted
//! keystore file (default `~/.config/degenbox/hl-keystore.json`).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub use degenbox_signer_core::hl::config::{executed_path_in, HlConfig as Config, NetworkChoice};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HOME not set")]
    NoHome,
}

/// Platform config directory: `%APPDATA%\degenbox` (Windows),
/// `$XDG_CONFIG_HOME/degenbox` or `~/.config/degenbox` (Unix/macOS).
/// Same resolution as `signer-core::paths::default_dir` — kept as a
/// thin wrapper so the CLI's `ConfigError` surface is unchanged.
pub fn default_dir() -> Result<PathBuf, ConfigError> {
    degenbox_signer_core::default_dir().map_err(|_| ConfigError::NoHome)
}

/// Returns the path to the `bots/` directory where each bot gets its own subfolder.
pub fn bots_dir() -> Result<PathBuf, ConfigError> {
    Ok(default_dir()?.join("bots"))
}

pub fn default_config_path() -> Result<PathBuf, ConfigError> {
    Ok(default_dir()?.join("hl-config.json"))
}

pub fn default_keystore_path() -> Result<PathBuf, ConfigError> {
    Ok(default_dir()?.join("hl-keystore.json"))
}

/// Local append-only audit log of every signed+submitted instruction.
pub fn default_audit_path() -> Result<PathBuf, ConfigError> {
    Ok(default_dir()?.join("audit.jsonl"))
}

/// Local executed-instruction marker store (idempotency ledger). Keyed on
/// instruction `cloid`; lets a `post_result` retry skip re-`execute`-ing an
/// order that already hit Hyperliquid.
///
/// NOTE: this global path is the LEGACY single-bot location. In the
/// multi-client hub every bot has its OWN config directory and MUST use
/// [`executed_path_in`] so two hub bots configured for the same user/signer
/// never share one in-memory marker map (a marker written by bot A's
/// `ExecutedStore` is otherwise invisible to bot B's separate map, so both
/// would `execute()` the same pending instruction → double-submit). Prefer
/// [`executed_path_in`] everywhere a per-bot directory is known.
pub fn default_executed_path() -> Result<PathBuf, ConfigError> {
    Ok(default_dir()?.join("executed.jsonl"))
}

/// Discovers all configured bots in the `bots/` directory.
/// Automatically migrates any legacy global config into `bots/default/`.
pub fn discover_bots() -> Result<Vec<(String, PathBuf)>, ConfigError> {
    let bots_root = bots_dir()?;
    let global_cfg = default_config_path()?;
    let global_key = default_keystore_path()?;

    // Auto-migrate legacy global files if they exist and the bot directory hasn't been created yet.
    if global_cfg.exists() && global_key.exists() {
        let default_bot_dir = bots_root.join("default");
        if !default_bot_dir.exists() {
            fs::create_dir_all(&default_bot_dir).ok();
            fs::rename(&global_cfg, default_bot_dir.join("hl-config.json")).ok();
            fs::rename(&global_key, default_bot_dir.join("hl-keystore.json")).ok();
            // Also move the audit log if present
            let global_audit = default_audit_path()?;
            if global_audit.exists() {
                fs::rename(&global_audit, default_bot_dir.join("audit.jsonl")).ok();
            }
        }
    }

    let mut bots = Vec::new();
    if !bots_root.exists() {
        return Ok(bots);
    }

    if let Ok(entries) = fs::read_dir(bots_root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let name = entry.file_name().to_string_lossy().to_string();
                let cfg_path = entry.path().join("hl-config.json");
                if cfg_path.exists() {
                    bots.push((name, entry.path()));
                }
            }
        }
    }
    Ok(bots)
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
            client_name: Some("My Scalp Bot".into()),
            poll_secs: 5,
            ..Config::default()
        };
        save(&path, &original).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.server_url, original.server_url);
        assert_eq!(loaded.api_token, original.api_token);
        assert_eq!(loaded.agent_address, original.agent_address);
        assert_eq!(loaded.account_address, original.account_address);
        assert_eq!(loaded.network, original.network);
        assert_eq!(loaded.host_id, original.host_id);
        assert_eq!(loaded.client_name, original.client_name);
        assert_eq!(loaded.poll_secs, original.poll_secs);
    }

    /// A config written before `poll_secs` existed must still load,
    /// defaulting the cadence to 3 rather than failing to deserialize.
    #[test]
    fn legacy_config_without_poll_secs_defaults_to_three() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        let legacy = serde_json::json!({
            "server_url": "https://x",
            "network": "mainnet",
        });
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.poll_secs, 3);
    }

    /// Old configs written before `account_address` existed must still
    /// load — `#[serde(default)]` absorbs the missing field.
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

    /// Two different bot directories MUST resolve to two different
    /// executed-marker files (see `default_executed_path` docs).
    #[test]
    fn executed_path_is_per_bot_dir() {
        let pa = executed_path_in(Path::new("/cfg/bots/alice"));
        let pb = executed_path_in(Path::new("/cfg/bots/bob"));
        assert_ne!(pa, pb, "distinct bot dirs must yield distinct marker files");
        assert_eq!(pa, Path::new("/cfg/bots/alice/executed.jsonl"));
        assert_eq!(pb, Path::new("/cfg/bots/bob/executed.jsonl"));
    }
}
