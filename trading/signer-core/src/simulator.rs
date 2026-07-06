//! Pre-sign simulation via Solana RPC `simulateTransaction`.
//!
//! The signer NEVER hits the chain blind. Before we sign a Jupiter-
//! supplied tx we ask the RPC node to dry-run it: if `err` is set, or
//! if any log line contains an error marker, we refuse to sign.
//!
//! ## Why pre-sign and not post-sign
//!
//! `simulateTransaction` can run against an unsigned tx if we pass
//! `sigVerify=false` and `replaceRecentBlockhash=true`. That avoids
//! committing the user's signature to a tx that's going to fail
//! anyway — a real concern when sniping fresh launches where the
//! pool may have liquidity but tax / freeze authority will revert
//! the swap.
//!
//! ## What this catches
//!
//! - `meta.err` set → tx would fail on chain. Reject.
//! - Logs contain `failed:`, `Error:`, `insufficient funds`, etc. —
//!   even on success the user almost never wants those.
//! - Compute-units consumed exceeds a soft cap (200k) — likely a
//!   bad route. Warn, don't reject (some legitimate Jupiter routes
//!   hit 180k+ CU on multi-hop swaps).
//!
//! ## What this does NOT catch
//!
//! - Price-impact above slippage. Jupiter's `/swap` already accounts
//!   for slippage, but a stale quote may produce more impact than
//!   simulated. `program_allow` + budget-guard layer on top.
//! - Front-runs between simulate and submit. The Falcon QUIC path
//!   has its own gas-priority race that's beyond this layer's
//!   responsibility.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Soft CU ceiling. Above this we warn; the caller decides whether
/// to reject. Jupiter's `dynamic_compute_unit_limit=true` typically
/// produces 30–80k for direct routes, 150–200k for multi-hop.
const CU_WARN_CEILING: u64 = 250_000;

#[derive(Debug, Error)]
pub enum SimulateError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("rpc returned status {0}: {1}")]
    Status(u16, String),
    #[error("rpc error: {0}")]
    RpcError(String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("simulation rejected: {0}")]
    Rejected(String),
}

#[derive(Clone)]
pub struct Simulator {
    http: reqwest::Client,
    rpc_url: String,
}

impl Simulator {
    pub fn new(rpc_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            rpc_url: rpc_url.into(),
        }
    }

    /// Simulate `signed_or_unsigned_b64`. When `sig_verify=false` we
    /// can pass an unsigned tx — Jupiter returns one with a zeroed
    /// signature placeholder, perfect for pre-sign dry-runs.
    pub async fn simulate(
        &self,
        tx_b64: &str,
        sig_verify: bool,
    ) -> Result<SimulationOutcome, SimulateError> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "simulateTransaction",
            "params": [
                tx_b64,
                {
                    "encoding": "base64",
                    "sigVerify": sig_verify,
                    "replaceRecentBlockhash": !sig_verify,
                    "commitment": "processed"
                }
            ]
        });
        let resp = self.http.post(&self.rpc_url).json(&req).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SimulateError::Status(status.as_u16(), body));
        }
        let parsed: RpcResp = resp
            .json()
            .await
            .map_err(|e| SimulateError::Decode(e.to_string()))?;
        if let Some(err) = parsed.error {
            return Err(SimulateError::RpcError(format!(
                "code={} msg={}",
                err.code, err.message
            )));
        }
        let result = parsed
            .result
            .ok_or_else(|| SimulateError::Decode("missing result".into()))?;
        Ok(analyse(result))
    }
}

/// Outcome a caller can branch on. `would_fail = true` means refuse
/// to sign. `warnings` is informational; CU-over-soft-cap shows up
/// here.
#[derive(Debug, Clone)]
pub struct SimulationOutcome {
    pub would_fail: bool,
    pub failure_reason: Option<String>,
    pub units_consumed: Option<u64>,
    pub warnings: Vec<String>,
    /// Raw logs from the RPC — useful for debugging when the simulator
    /// rejects something the user thinks should pass.
    pub logs: Vec<String>,
}

fn analyse(result: SimResult) -> SimulationOutcome {
    let value = result.value;
    let mut warnings = Vec::new();

    // 1. Hard fail: `err` set.
    if let Some(err) = value.err.as_ref() {
        return SimulationOutcome {
            would_fail: true,
            failure_reason: Some(format!("rpc err: {err}")),
            units_consumed: value.units_consumed,
            warnings,
            logs: value.logs.clone().unwrap_or_default(),
        };
    }

    // 2. Hard fail: any log line that smells like an error. We avoid
    //    matching on `"log: AnchorError"` substrings inside a happy-
    //    path log by requiring one of a closed set of leading markers.
    let logs = value.logs.clone().unwrap_or_default();
    if let Some(bad) = first_error_log(&logs) {
        return SimulationOutcome {
            would_fail: true,
            failure_reason: Some(format!("log: {bad}")),
            units_consumed: value.units_consumed,
            warnings,
            logs,
        };
    }

    // 3. Soft warn: CU above the ceiling.
    if let Some(cu) = value.units_consumed {
        if cu > CU_WARN_CEILING {
            warnings.push(format!(
                "compute units {cu} exceeds soft ceiling {CU_WARN_CEILING}"
            ));
        }
    }

    SimulationOutcome {
        would_fail: false,
        failure_reason: None,
        units_consumed: value.units_consumed,
        warnings,
        logs,
    }
}

/// Return the first log entry that looks like a failure marker.
/// Closed set of markers — we intentionally don't pattern-match on
/// "Error" substrings because Solana programs emit lots of legitimate
/// info-level logs that contain that word (e.g.
/// "Program log: error_handler_v2 entry").
fn first_error_log(logs: &[String]) -> Option<String> {
    const MARKERS: &[&str] = &[
        "Program log: failed:",
        "Program log: Error:",
        "Program log: insufficient",
        "Transfer: insufficient",
        "AnchorError occurred",
        "Program failed to complete",
        "Custom program error:",
    ];
    for line in logs {
        for m in MARKERS {
            if line.contains(m) {
                return Some(line.clone());
            }
        }
    }
    None
}

// ─── wire types ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RpcResp {
    #[allow(dead_code)]
    jsonrpc: String,
    result: Option<SimResult>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct SimResult {
    #[allow(dead_code)]
    context: Option<serde_json::Value>,
    value: SimValue,
}

#[derive(Debug, Deserialize)]
struct SimValue {
    /// Solana wraps errors in heterogeneous shapes (string, object).
    /// We treat anything non-null as "tx failed".
    #[serde(default)]
    err: Option<serde_json::Value>,
    #[serde(default)]
    logs: Option<Vec<String>>,
    #[serde(default, rename = "unitsConsumed")]
    units_consumed: Option<u64>,
    // Other fields (accounts, returnData, replacementBlockhash) we
    // don't currently use; serde drops them silently.
    #[allow(dead_code)]
    #[serde(flatten, default)]
    _other: serde_json::Map<String, serde_json::Value>,
}

/// `Serialize` so callers can persist outcomes for audit log.
impl Serialize for SimulationOutcome {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("SimulationOutcome", 5)?;
        st.serialize_field("would_fail", &self.would_fail)?;
        st.serialize_field("failure_reason", &self.failure_reason)?;
        st.serialize_field("units_consumed", &self.units_consumed)?;
        st.serialize_field("warnings", &self.warnings)?;
        st.serialize_field("logs", &self.logs)?;
        st.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn happy_result() -> SimResult {
        SimResult {
            context: None,
            value: SimValue {
                err: None,
                logs: Some(vec![
                    "Program ComputeBudget111... invoke [1]".into(),
                    "Program log: Instruction: Swap".into(),
                    "Program log: Routed via Whirlpool".into(),
                    "Program ComputeBudget111... success".into(),
                ]),
                units_consumed: Some(48_000),
                _other: Default::default(),
            },
        }
    }

    #[test]
    fn happy_path_passes() {
        let outcome = analyse(happy_result());
        assert!(!outcome.would_fail);
        assert!(outcome.failure_reason.is_none());
        assert_eq!(outcome.units_consumed, Some(48_000));
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn rpc_err_rejects() {
        let r = SimResult {
            context: None,
            value: SimValue {
                err: Some(serde_json::json!({"InstructionError":[0,"InvalidProgramId"]})),
                logs: Some(vec!["Program log: ok".into()]),
                units_consumed: Some(0),
                _other: Default::default(),
            },
        };
        let outcome = analyse(r);
        assert!(outcome.would_fail);
        assert!(outcome.failure_reason.as_ref().unwrap().contains("rpc err"));
    }

    #[test]
    fn error_log_rejects() {
        let r = SimResult {
            context: None,
            value: SimValue {
                err: None,
                logs: Some(vec![
                    "Program ComputeBudget111... invoke [1]".into(),
                    "Program log: AnchorError occurred. Error Code: SlippageExceeded.".into(),
                    "Program ComputeBudget111... failed".into(),
                ]),
                units_consumed: Some(30_000),
                _other: Default::default(),
            },
        };
        let outcome = analyse(r);
        assert!(outcome.would_fail);
        assert!(outcome
            .failure_reason
            .as_ref()
            .unwrap()
            .contains("AnchorError"));
    }

    #[test]
    fn high_cu_warns_but_passes() {
        let mut r = happy_result();
        r.value.units_consumed = Some(280_000);
        let outcome = analyse(r);
        assert!(!outcome.would_fail);
        assert!(!outcome.warnings.is_empty());
        assert!(outcome.warnings[0].contains("280000"));
    }

    #[test]
    fn legitimate_log_containing_error_word_is_not_a_match() {
        // We don't substring-match on bare "error" — only on the
        // closed set of leading markers.
        let r = SimResult {
            context: None,
            value: SimValue {
                err: None,
                logs: Some(vec![
                    "Program log: error_recovery_path skipped (happy path)".into(),
                ]),
                units_consumed: Some(48_000),
                _other: Default::default(),
            },
        };
        let outcome = analyse(r);
        assert!(!outcome.would_fail, "false-positive on legitimate log");
    }

    #[test]
    fn insufficient_funds_is_caught() {
        let r = SimResult {
            context: None,
            value: SimValue {
                err: None,
                logs: Some(vec![
                    "Program log: Instruction: Transfer".into(),
                    "Transfer: insufficient lamports 1000, need 2000000".into(),
                ]),
                units_consumed: Some(8_000),
                _other: Default::default(),
            },
        };
        let outcome = analyse(r);
        assert!(outcome.would_fail);
    }
}
