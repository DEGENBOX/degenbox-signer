//! On-disk HL daemon config shared by every signer front-end.
//!
//! Wire-compatible with the `hl-signer-desktop` CLI's `hl-config.json`
//! (same path via [`crate::paths::hl_config_path`]), so a user who runs
//! the CLI and the desktop app sees one config. Holds the DegenBox server
//! URL, the signer API token (a JWT minted by `redeem-registration` or a
//! long-lived bearer), the agent address (so the daemon can refuse to
//! sign if the keystore unlocks a different one), and the user's HL
//! MASTER account (`0x…`) — required for `closePosition` / `placeTpsl`
//! position lookups and for the gateway's `paired_with_account` gate.
//!
//! The private key NEVER lives here — it stays in the encrypted
//! keystore ([`crate::paths::hl_keystore_path`]).

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NetworkChoice {
    Mainnet,
    Testnet,
}

impl Default for NetworkChoice {
    fn default() -> Self {
        Self::Mainnet
    }
}

/// HL daemon config. Mirrors the CLI's `config::Config` field set so the
/// JSON file round-trips between the two front-ends. Unknown extra fields
/// the CLI writes (e.g. `client_name`) are tolerated via `extra`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlConfig {
    pub server_url: String,
    /// Signer token: either a JWT minted by `redeem-registration`, or a
    /// long-lived API token. Sent as `Authorization: Bearer …`.
    #[serde(default)]
    pub api_token: Option<String>,
    #[serde(default)]
    pub agent_address: Option<String>,
    /// User's HL MASTER account (the 0x… wallet the agent acts for).
    /// Required by `closePosition` / `placeTpsl` and the gateway's
    /// pairing gate. Distinct from `agent_address`.
    #[serde(default)]
    pub account_address: Option<String>,
    #[serde(default)]
    pub network: NetworkChoice,
    #[serde(default)]
    pub host_id: Option<String>,
    /// Operator-chosen display name for this bot instance — shown in the
    /// CLI's multi-client hub dashboard. Promoted from the flattened
    /// `extra` map (where the app build used to absorb it) so the unified
    /// CLI can read/write it typed; the JSON wire shape is unchanged.
    #[serde(default)]
    pub client_name: Option<String>,
    /// Per-bot poll cadence (seconds) for the server-poll fallback loop.
    /// Defaults to 3 to match the CLI when absent.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    /// Optional NATS URL for sub-second push nudges. Usually unset in the
    /// desktop app (poll-only is fine); the CLI sets it when given.
    #[serde(default)]
    pub nats_url: Option<String>,
    /// Paper-mode dry-run: resolve + log every instruction but NEVER POST
    /// to HL. A real safety control, persisted so it survives restart.
    #[serde(default)]
    pub paper_mode: bool,
    /// Tolerate forward-compatible keys the CLI may add without failing
    /// to deserialize.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_poll_secs() -> u64 {
    3
}

impl Default for HlConfig {
    fn default() -> Self {
        Self {
            server_url: "https://api-v2.degenbox.app".into(),
            api_token: None,
            agent_address: None,
            account_address: None,
            network: NetworkChoice::Mainnet,
            host_id: None,
            client_name: None,
            poll_secs: default_poll_secs(),
            nats_url: None,
            paper_mode: false,
            extra: serde_json::Map::new(),
        }
    }
}

impl HlConfig {
    /// Load the config at the shared HL config path, or `Default` when it
    /// doesn't exist / can't be parsed (a fresh install).
    pub fn load_or_default() -> Self {
        let Ok(path) = crate::paths::hl_config_path() else {
            return Self::default();
        };
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Atomically persist to the shared HL config path with 0600 perms.
    pub fn save(&self) -> Result<(), String> {
        let path = crate::paths::hl_config_path().map_err(|e| e.to_string())?;
        save_to(&path, self)
    }

    /// Load a config from an arbitrary path (per-wallet vault configs),
    /// or `Default` when absent/unreadable.
    pub fn load_from(path: &Path) -> Self {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Atomically persist to an arbitrary path (per-wallet vault
    /// configs) with 0600 perms.
    pub fn save_to_path(&self, path: &Path) -> Result<(), String> {
        save_to(path, self)
    }
}

fn save_to(path: &Path, cfg: &HlConfig) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(cfg).map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    tmp.write_all(&bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600));
    }
    tmp.persist(path).map_err(|e| e.error.to_string())?;
    Ok(())
}

/// Local executed-instruction marker store path (idempotency ledger).
/// Same filename the CLI's single-bot install uses so a user flipping
/// between front-ends keeps one idempotency record.
pub fn executed_path() -> Result<std::path::PathBuf, String> {
    Ok(crate::paths::default_dir()
        .map_err(|e| e.to_string())?
        .join("executed.jsonl"))
}

/// Append-only local audit log of every signed + submitted instruction.
/// Same filename as the CLI so the user has ONE local record regardless
/// of which front-end signed.
pub fn audit_path() -> Result<std::path::PathBuf, String> {
    Ok(crate::paths::default_dir()
        .map_err(|e| e.to_string())?
        .join("audit.jsonl"))
}

/// Per-bot executed-instruction marker store: the idempotency ledger
/// lives inside the SAME directory as the bot's `hl-config.json` /
/// `hl-keystore.json` so each multi-client hub bot owns a distinct
/// `<bot_dir>/executed.jsonl` and two bots configured for the same
/// user/signer never share one in-memory marker map (which would defeat
/// the idempotency poison-pill and allow a double-submit). Ported from
/// the prod CLI (`hl-signer-desktop`); [`executed_path`] is the legacy
/// single-instance global fallback.
pub fn executed_path_in(dir: &Path) -> std::path::PathBuf {
    dir.join("executed.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_cli_config_without_app_fields_loads() {
        // A config written by the CLI (has client_name / poll_secs, lacks
        // paper_mode / nats_url) must still deserialize, defaulting the
        // app-only fields and absorbing the unknown ones into `extra`.
        let legacy = serde_json::json!({
            "server_url": "https://x",
            "api_token": "tok",
            "agent_address": "0xdead",
            "account_address": "0xbeef",
            "network": "testnet",
            "host_id": null,
            "client_name": "My Scalp Bot",
            "poll_secs": 5,
        });
        let cfg: HlConfig = serde_json::from_value(legacy).unwrap();
        assert_eq!(cfg.poll_secs, 5);
        assert_eq!(cfg.network, NetworkChoice::Testnet);
        assert!(!cfg.paper_mode);
        assert!(cfg.nats_url.is_none());
        assert_eq!(cfg.account_address.as_deref(), Some("0xbeef"));
        // `client_name` is now a TYPED field (the unified CLI reads it),
        // no longer absorbed into `extra`.
        assert_eq!(cfg.client_name.as_deref(), Some("My Scalp Bot"));
        assert!(!cfg.extra.contains_key("client_name"));
    }

    #[test]
    fn roundtrip_preserves_app_fields() {
        let cfg = HlConfig {
            server_url: "https://api-v2.degenbox.app".into(),
            api_token: Some("jwt".into()),
            agent_address: Some("0xa".into()),
            account_address: Some("0xb".into()),
            network: NetworkChoice::Mainnet,
            host_id: Some("mac".into()),
            client_name: None,
            poll_secs: 3,
            nats_url: None,
            paper_mode: true,
            extra: serde_json::Map::new(),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: HlConfig = serde_json::from_str(&s).unwrap();
        assert!(back.paper_mode);
        assert_eq!(back.api_token.as_deref(), Some("jwt"));
    }

    #[test]
    fn cli_extra_fields_roundtrip_through_save_shape() {
        // The flattened `extra` map must survive a serialize→deserialize
        // cycle so an app-side `save()` never strips forward-compatible
        // keys other clients may add to the shared file. (`client_name`
        // itself is typed now and must ALSO round-trip.)
        let legacy = serde_json::json!({
            "server_url": "https://x",
            "network": "mainnet",
            "client_name": "Swing Bot",
            "some_future_key": "kept",
        });
        let cfg: HlConfig = serde_json::from_value(legacy).unwrap();
        let s = serde_json::to_string(&cfg).unwrap();
        let back: HlConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.client_name.as_deref(), Some("Swing Bot"));
        assert_eq!(
            back.extra.get("some_future_key").and_then(|v| v.as_str()),
            Some("kept")
        );
    }

    #[test]
    fn executed_path_is_per_bot_dir() {
        // Two different bot dirs MUST resolve to two different marker
        // files (prod CLI invariant — see fn docs).
        let pa = executed_path_in(Path::new("/cfg/bots/alice"));
        let pb = executed_path_in(Path::new("/cfg/bots/bob"));
        assert_ne!(pa, pb);
        assert_eq!(pa, Path::new("/cfg/bots/alice/executed.jsonl"));
    }
}
