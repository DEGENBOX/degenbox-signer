//! Local append-only JSONL audit log.
//!
//! Every instruction the signer signs + POSTs to HL (and every offline
//! `panic` action) is recorded as one JSON line in
//! `~/.config/degenbox/audit.jsonl` (0600 on Unix). This is the user's
//! own tamper-evident record of what their key signed — independent of
//! the server's `hl_exchange_orders` view. Canonical copy of
//! `hl-signer-desktop/src/audit.rs` (which itself ports the v1 Go
//! client's `internal/relay/auditlog.go`).
//!
//! Best-effort: a write failure is logged and swallowed — auditing must
//! never block or fail a trade.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One audit line. Flat + stable so the file stays `jq`-friendly.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    /// RFC3339 timestamp the line was written.
    pub ts: DateTime<Utc>,
    /// Where the action came from: `"daemon"` (server-queued) or
    /// `"panic"` (local offline kill-switch).
    pub source: &'static str,
    /// HL client order id (or our synthetic cancel/close id).
    pub cloid: String,
    /// Payload kind: `order` | `cancel` | `closePosition` | `placeTpsl`
    /// | `updateLeverage` | `vaultTransfer` | `trailingSl` | `panic_*`.
    pub kind: String,
    /// Asset symbol when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// Terminal status reported back: `submitted` | `filled` |
    /// `cancelled` | `failed`.
    pub status: String,
    /// HL order id when the action produced one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oid: Option<i64>,
    /// Error string when `status == "failed"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Append-only JSONL writer. Cheap to clone-share via `Arc`.
pub struct AuditLog {
    path: PathBuf,
    lock: Mutex<()>,
}

impl AuditLog {
    /// Open (creating if needed) the audit log at `path`. Ensures the
    /// parent dir exists and applies 0600 perms on Unix.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Touch the file so perms can be set even on first run.
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        drop(f);
        Ok(Self {
            path: path.to_path_buf(),
            lock: Mutex::new(()),
        })
    }

    /// Append one entry. Errors are returned so callers can log them;
    /// they must not propagate into the trade path.
    pub fn record(&self, entry: &AuditEntry) -> std::io::Result<()> {
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        writeln!(f, "{line}")?;
        f.flush()
    }

    /// Append, logging (not propagating) any IO error. The convenience
    /// wrapper for the hot path — auditing is best-effort.
    pub fn record_lossy(&self, entry: &AuditEntry) {
        if let Err(e) = self.record(entry) {
            tracing::warn!(?e, "audit log write failed (non-fatal)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_one_json_line_per_entry() {
        let dir = std::env::temp_dir().join(format!("dbx-audit-test-{}", std::process::id()));
        let path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let log = AuditLog::open(&path).unwrap();
        for i in 0..3 {
            log.record(&AuditEntry {
                ts: DateTime::<Utc>::UNIX_EPOCH,
                source: "daemon",
                cloid: format!("cl{i}"),
                kind: "order".into(),
                asset: Some("BTC".into()),
                status: "filled".into(),
                oid: Some(i),
                error: None,
            })
            .unwrap();
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3);
        // Each line is independently valid JSON.
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["source"], "daemon");
            assert_eq!(v["kind"], "order");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn omits_none_fields() {
        let e = AuditEntry {
            ts: DateTime::<Utc>::UNIX_EPOCH,
            source: "panic",
            cloid: "x".into(),
            kind: "panic_close".into(),
            asset: None,
            status: "filled".into(),
            oid: None,
            error: None,
        };
        let s = serde_json::to_string(&e).unwrap();
        assert!(!s.contains("\"asset\""));
        assert!(!s.contains("\"oid\""));
        assert!(!s.contains("\"error\""));
    }
}
