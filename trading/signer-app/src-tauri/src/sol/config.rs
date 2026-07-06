//! Persisted Solana-execution settings for the app — `sol-config.json`
//! next to the shared keystores (`~/.config/degenbox/` on Unix).
//!
//! v0.3.0 slice 8: the per-unlock client budget (`copy_session_sol` /
//! `copy_per_token_sol`) is RETIRED as a gate — copy budgets are
//! per-config server-side (`sol_copy_trade_configs.copy_budget_*`,
//! slice 6) and arrive folded into each event's `max_spend_lamports`,
//! which the signer clamps to. The fields stay parsed for config-file
//! back-compat but no longer arm/disarm anything; the slice-9 UI
//! removes them.

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolConfig {
    /// RETIRED (slice 8): former per-unlock client spend cap. Kept so
    /// existing config files round-trip; no longer gates anything.
    #[serde(default)]
    pub copy_session_sol: Option<f64>,
    /// RETIRED (slice 8): former per-token client cap. Kept for
    /// config-file back-compat only.
    #[serde(default)]
    pub copy_per_token_sol: Option<f64>,
    /// Default slippage for sells (copy buys carry per-event slippage).
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: u16,
    #[serde(default = "default_tip_lamports")]
    pub tip_lamports: i64,
    #[serde(default = "default_submit_mode")]
    pub submit_mode: String,
    /// Solana RPC override. Falls back to `SOLANA_RPC_URL`, then public
    /// mainnet-beta.
    #[serde(default)]
    pub rpc_url: Option<String>,
    /// Forward-compatible keys other clients may add.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_slippage_bps() -> u16 {
    100
}
fn default_tip_lamports() -> i64 {
    1_000_000
}
fn default_submit_mode() -> String {
    "falcon_jito".into()
}

impl Default for SolConfig {
    fn default() -> Self {
        Self {
            copy_session_sol: None,
            copy_per_token_sol: None,
            slippage_bps: default_slippage_bps(),
            tip_lamports: default_tip_lamports(),
            submit_mode: default_submit_mode(),
            rpc_url: None,
            extra: serde_json::Map::new(),
        }
    }
}

fn config_path() -> Result<PathBuf, String> {
    Ok(degenbox_signer_core::default_dir()
        .map_err(|e| e.to_string())?
        .join("sol-config.json"))
}

impl SolConfig {
    pub fn load_or_default() -> Self {
        let Ok(path) = config_path() else {
            return Self::default();
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    /// Atomic write (tempfile + rename) so a crash mid-write can't
    /// corrupt the config. 0600 on Unix — same pattern as `HlConfig`.
    pub fn save(&self) -> Result<(), String> {
        let path = config_path()?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        tmp.write_all(&bytes).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600));
        }
        tmp.persist(&path).map_err(|e| e.error.to_string())?;
        Ok(())
    }

    /// The user's explicit RPC choice: config override first, then the
    /// `SOLANA_RPC_URL` env. `None` = no explicit choice → the caller
    /// should prefer the gateway RPC proxy (zero-config default).
    pub fn rpc_override(&self) -> Option<String> {
        self.rpc_url
            .clone()
            .or_else(|| std::env::var("SOLANA_RPC_URL").ok())
    }

    /// Full resolution chain (v0.3.0 slice 8): user override →
    /// `SOLANA_RPC_URL` env → the gateway's token-gated Solana RPC
    /// proxy (zero-config default) → public mainnet-beta when no
    /// gateway credentials exist either.
    pub fn resolved_rpc_url_with_auth(&self, auth: Option<(&str, &str)>) -> String {
        self.rpc_override().unwrap_or_else(|| match auth {
            Some((base, token)) => degenbox_signer_core::gateway_proxy_rpc_url(base, token),
            None => "https://api.mainnet-beta.solana.com".into(),
        })
    }
}
