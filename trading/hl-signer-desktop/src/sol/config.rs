//! Persisted Solana-execution settings — `sol-config.json` next to the
//! shared keystores (`~/.config/degenbox/` on Unix). SAME file + shape
//! as the Tauri desktop app, so a user flipping between the unified CLI
//! and the app keeps one budget.
//!
//! The important field is `copy_session_sol`: the hard CLIENT-side
//! spend ceiling for unattended copy-trade buys per process session.
//! It mirrors `signer-cli watch-copy --session-sol`, which is a
//! REQUIRED argument there — we keep it mandatory here by never
//! defaulting it: while it is unset, copy BUYS are refused (with a
//! logged + visible reason); TP/SL sells and mirror-sells still run
//! because they only dispose of tokens, never spend SOL.

use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolConfig {
    /// Hard client-side SOL spend cap for copy buys per session.
    /// `None` = copy buys disarmed (sells unaffected).
    #[serde(default)]
    pub copy_session_sol: Option<f64>,
    /// Optional per-token client-side cap (SOL) on top of the session cap.
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
    /// corrupt the config. 0600 on Unix — same pattern as `hl-config`.
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

    pub fn resolved_rpc_url(&self) -> String {
        self.rpc_url
            .clone()
            .or_else(|| std::env::var("SOLANA_RPC_URL").ok())
            .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".into())
    }
}
