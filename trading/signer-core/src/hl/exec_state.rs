//! Local executed-instruction marker store — the signer's idempotency
//! ledger across `post_result` retries.
//!
//! ## Why this exists (money bug)
//!
//! `handle_one` does two things that must NOT be coupled: (1) `execute`
//! signs + POSTs the order/cancel/closePosition to Hyperliquid, and (2)
//! `post_result` reports the outcome to the DegenBox gateway. If (1)
//! SUCCEEDS but (2) fails (a gateway outage, network blip), `handle_one`
//! returns `Err`, the poll-loop cursor does NOT advance past the row, and
//! on the next poll the SAME row is re-fetched — re-running `execute` and
//! **re-submitting the order/cancel/close to HL**. HL's cloid dedup only
//! protects within a short window, so a long gateway outage can re-fire a
//! stored close/cancel for real money.
//!
//! The fix: persist (locally, on disk) that `execute` already SUCCEEDED
//! for a row BEFORE attempting `post_result`. On a retry the daemon sees
//! the marker, SKIPS re-execution, and only retries `post_result` with the
//! cached result. The marker is keyed on the instruction `cloid` (stable,
//! unique per instruction), so it survives a process restart too.
//!
//! Best-effort durability with a strong bias to safety: if the marker
//! WRITE fails we surface it (the caller logs loudly) but still proceed —
//! we never want a disk error to block a protective close. The asymmetry
//! is deliberate: a missing marker can only cause an at-most-once-more
//! re-submit (the pre-existing risk we are reducing), never a dropped
//! trade.

use crate::hl::signing::SignedSubmitResult;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One persisted marker line: a cloid that already executed against HL,
/// plus the cached result so a `post_result` retry can re-report it
/// verbatim without re-submitting.
///
/// H9: `undelivered` marks a result whose `post_result` exhausted its
/// retries — the daemon CONTINUES with the batch and re-delivers it from
/// this store on later poll cycles instead of wedging. `serde(default)`
/// keeps every pre-existing marker line (no field) decoding as
/// delivered, so an upgrade NEVER re-POSTs historical results (which
/// could clobber a newer order status on the gateway).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MarkerLine {
    cloid: String,
    result: SignedSubmitResult,
    #[serde(default)]
    undelivered: bool,
}

/// Tombstone line appended when a previously-undelivered result finally
/// reached the gateway: `{"cloid": "...", "reported": true}`. Old signer
/// binaries skip it as a malformed `MarkerLine` (missing `result`) —
/// forward compatible.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReportedLine {
    cloid: String,
    reported: bool,
}

/// Append-only, restart-durable store of cloids whose `execute` already
/// SUCCEEDED. Loaded into an in-memory map on open (the daemon is a single
/// long-running process) and appended-to as new instructions execute.
pub struct ExecutedStore {
    path: PathBuf,
    inner: Mutex<HashMap<String, SignedSubmitResult>>,
    /// H9: cloids whose result is executed-but-undelivered (gateway POST
    /// kept failing). Re-delivered by the daemon's flush pass.
    undelivered: Mutex<HashMap<String, SignedSubmitResult>>,
}

impl ExecutedStore {
    /// Open (creating if needed) the store at `path`, replaying any
    /// existing lines into memory. Ensures the parent dir exists and
    /// applies 0600 perms on Unix. A malformed line is skipped (never
    /// fatal) so a partial/corrupt tail can't brick the daemon.
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
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

        let mut map = HashMap::new();
        let mut undelivered: HashMap<String, SignedSubmitResult> = HashMap::new();
        if let Ok(contents) = std::fs::read_to_string(path) {
            for line in contents.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(m) = serde_json::from_str::<MarkerLine>(line) {
                    if m.undelivered {
                        undelivered.insert(m.cloid.clone(), m.result.clone());
                    }
                    map.insert(m.cloid, m.result);
                } else if let Ok(r) = serde_json::from_str::<ReportedLine>(line) {
                    // Delivery tombstone — clears the undelivered flag.
                    if r.reported {
                        undelivered.remove(&r.cloid);
                    }
                }
            }
        }

        Ok(Self {
            path: path.to_path_buf(),
            inner: Mutex::new(map),
            undelivered: Mutex::new(undelivered),
        })
    }

    /// Return the cached result for a cloid whose `execute` already
    /// SUCCEEDED, or `None` when this instruction has not been executed
    /// yet (the normal first-time path).
    pub fn get(&self, cloid: &str) -> Option<SignedSubmitResult> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(cloid)
            .cloned()
    }

    /// Persist that `cloid` executed successfully, caching its result.
    /// Called BEFORE `post_result` so a `post_result` failure → re-poll
    /// can never re-execute. Errors are returned so the caller can log
    /// them; the trade path treats this as best-effort.
    pub fn mark_executed(&self, cloid: &str, result: &SignedSubmitResult) -> std::io::Result<()> {
        // In-memory first so a concurrent/subsequent in-process `get`
        // sees it even if the disk write lags.
        {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(cloid.to_string(), result.clone());
        }
        self.append_line(&MarkerLine {
            cloid: cloid.to_string(),
            result: result.clone(),
            undelivered: false,
        })
    }

    /// H9: persist a result whose `post_result` exhausted its retries so
    /// the daemon can CONTINUE with the batch and re-deliver this result
    /// on later poll cycles (instead of `break`ing and stranding every
    /// younger instruction). Also (re-)marks the cloid executed.
    pub fn mark_undelivered(
        &self,
        cloid: &str,
        result: &SignedSubmitResult,
    ) -> std::io::Result<()> {
        {
            let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(cloid.to_string(), result.clone());
        }
        {
            let mut und = self.undelivered.lock().unwrap_or_else(|e| e.into_inner());
            und.insert(cloid.to_string(), result.clone());
        }
        self.append_line(&MarkerLine {
            cloid: cloid.to_string(),
            result: result.clone(),
            undelivered: true,
        })
    }

    /// H9: a previously-undelivered result finally reached the gateway —
    /// clear it from the re-delivery set (durable tombstone).
    pub fn mark_reported(&self, cloid: &str) -> std::io::Result<()> {
        {
            let mut und = self.undelivered.lock().unwrap_or_else(|e| e.into_inner());
            und.remove(cloid);
        }
        let line = serde_json::to_string(&ReportedLine {
            cloid: cloid.to_string(),
            reported: true,
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.append_raw(&line)
    }

    /// Executed results still awaiting gateway delivery, oldest-first
    /// irrelevant (each is keyed on a unique cloid; the gateway ack is
    /// idempotent and order-independent).
    pub fn undelivered(&self) -> Vec<(String, SignedSubmitResult)> {
        self.undelivered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Whether `cloid` currently awaits re-delivery.
    pub fn is_undelivered(&self, cloid: &str) -> bool {
        self.undelivered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(cloid)
    }

    fn append_line(&self, line: &MarkerLine) -> std::io::Result<()> {
        let s = serde_json::to_string(line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.append_raw(&s)
    }

    fn append_raw(&self, line: &str) -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        writeln!(f, "{line}")?;
        f.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_result(cloid: &str) -> SignedSubmitResult {
        SignedSubmitResult {
            cloid: cloid.to_string(),
            oid: Some(42),
            status: "filled".into(),
            filled_size_usd: Some("100".into()),
            closed_pnl: None,
            err_msg: None,
        }
    }

    fn tmp_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "dbx-core-execstate-{}-{}-{}.jsonl",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn unmarked_cloid_returns_none() {
        let path = tmp_path("unmarked");
        let store = ExecutedStore::open(&path).unwrap();
        assert!(store.get("0xabc").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn marked_cloid_returns_cached_result() {
        let path = tmp_path("marked");
        let store = ExecutedStore::open(&path).unwrap();
        store
            .mark_executed("0xabc", &sample_result("0xabc"))
            .unwrap();
        let got = store.get("0xabc").expect("cached result");
        assert_eq!(got.cloid, "0xabc");
        assert_eq!(got.oid, Some(42));
        assert_eq!(got.status, "filled");
        let _ = std::fs::remove_file(&path);
    }

    /// THE money-bug regression: a marker persisted before a (simulated)
    /// post_result failure survives a process restart, so a re-open + get
    /// returns the cached result → the daemon skips re-execution.
    #[test]
    fn marker_survives_reopen_so_reexec_is_skipped() {
        let path = tmp_path("reopen");
        {
            let store = ExecutedStore::open(&path).unwrap();
            store
                .mark_executed("0xclose", &sample_result("0xclose"))
                .unwrap();
        }
        // Simulate the daemon crashing/restarting after execute succeeded
        // but before the row's cursor advanced.
        let reopened = ExecutedStore::open(&path).unwrap();
        let got = reopened
            .get("0xclose")
            .expect("marker must survive restart so closePosition is NOT re-fired");
        assert_eq!(got.status, "filled");
        // A different, never-executed instruction is still a clean miss.
        assert!(reopened.get("0xother").is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_line_is_skipped_not_fatal() {
        let path = tmp_path("malformed");
        std::fs::write(&path, "not-json\n").unwrap();
        let store = ExecutedStore::open(&path).expect("open must tolerate a corrupt line");
        assert!(store.get("anything").is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// H9: an undelivered result survives a restart and is re-offered for
    /// delivery; once reported, the tombstone clears it durably.
    #[test]
    fn undelivered_result_survives_restart_until_reported() {
        let path = tmp_path("undelivered");
        {
            let store = ExecutedStore::open(&path).unwrap();
            store
                .mark_undelivered("0xstuck", &sample_result("0xstuck"))
                .unwrap();
            assert!(store.is_undelivered("0xstuck"));
            // Still replayable as executed (idempotency intact).
            assert_eq!(store.get("0xstuck").unwrap().status, "filled");
        }
        // Restart: pending delivery persisted.
        let store = ExecutedStore::open(&path).unwrap();
        let pending = store.undelivered();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "0xstuck");
        // Deliver + tombstone.
        store.mark_reported("0xstuck").unwrap();
        assert!(!store.is_undelivered("0xstuck"));
        // Restart again: tombstone holds, executed marker still present.
        let store = ExecutedStore::open(&path).unwrap();
        assert!(store.undelivered().is_empty());
        assert!(store.get("0xstuck").is_some());
        let _ = std::fs::remove_file(&path);
    }

    /// H9 upgrade-safety: legacy marker lines (no `undelivered` field)
    /// must NEVER be treated as awaiting delivery — re-POSTing ancient
    /// results could clobber newer gateway order statuses.
    #[test]
    fn legacy_marker_lines_are_not_redelivered() {
        let path = tmp_path("legacy");
        std::fs::write(
            &path,
            r#"{"cloid":"0xold","result":{"cloid":"0xold","oid":1,"status":"filled","filled_size_usd":"5","closed_pnl":null,"err_msg":null}}"#,
        )
        .unwrap();
        let store = ExecutedStore::open(&path).unwrap();
        assert!(store.get("0xold").is_some(), "still an executed marker");
        assert!(
            store.undelivered().is_empty(),
            "legacy lines default to delivered"
        );
        let _ = std::fs::remove_file(&path);
    }
}
