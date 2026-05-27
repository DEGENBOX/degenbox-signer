//! HL `/exchange` POST client.
//!
//! Mirrors the Go reference in `legay-hyperliquid-bot/degenbox-client/
//! internal/hyperliquid/client.go` — same nonce semantics, same error
//! parsing, same `{action, nonce, signature, vaultAddress?}` envelope.

use crate::actions::{
    ApproveAgentAction, CancelAction, CancelByCloidAction, OrderAction, UpdateLeverageAction,
    VaultTransferAction,
};
use crate::signer::{AgentSigner, Network, SignerError};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ExchangeError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("http status {0}: {1}")]
    Status(u16, String),
    #[error("decode: {0}")]
    Decode(String),
    #[error("signing: {0}")]
    Sign(#[from] SignerError),
    #[error("hl api: {0}")]
    Api(String),
}

/// `{r, s, v}` Ethereum-style signature as required by HL `/exchange`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub r: String,
    pub s: String,
    pub v: u8,
}

#[derive(Debug, Serialize)]
struct ExchangeRequest<'a, T: Serialize> {
    action: &'a T,
    nonce: u64,
    signature: &'a Signature,
    #[serde(rename = "vaultAddress", skip_serializing_if = "Option::is_none")]
    vault_address: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExchangeResponse {
    pub status: String,
    #[serde(default)]
    pub response: Option<serde_json::Value>,
}

/// Per-order status returned in `response.data.statuses[…]`.
#[derive(Clone)]
pub enum OrderStatusEntry {
    Resting {
        oid: u64,
    },
    Filled {
        oid: u64,
        total_sz: String,
        avg_px: String,
    },
    Error(String),
}

/// Parsed result of a single `order` action with one order. The HL
/// response shape is `{response: {type, data: {statuses: [...]}}}`.
#[derive(Debug, Clone)]
pub struct OrderResult {
    pub statuses: Vec<OrderStatusEntry>,
}

#[derive(Debug, Clone)]
pub struct CancelResult {
    pub statuses: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ApprovalResult {
    pub status: String,
}

#[derive(Clone)]
pub struct ExchangeClient {
    http: Arc<reqwest::Client>,
    base: String,
    nonce: Arc<AtomicI64>,
    network: Network,
}

impl ExchangeClient {
    pub fn new(network: Network) -> Result<Self, ExchangeError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            http: Arc::new(http),
            base: network.exchange_url().to_string(),
            nonce: Arc::new(AtomicI64::new(0)),
            network,
        })
    }

    /// Inject a pre-built `reqwest::Client` (used by tests with custom
    /// connection pools, or callers that share one HTTP client across
    /// many `ExchangeClient` instances).
    pub fn with_client(
        network: Network,
        http: Arc<reqwest::Client>,
        base_override: Option<String>,
    ) -> Self {
        Self {
            http,
            base: base_override.unwrap_or_else(|| network.exchange_url().to_string()),
            nonce: Arc::new(AtomicI64::new(0)),
            network,
        }
    }

    pub fn network(&self) -> Network {
        self.network
    }

    /// Monotonic ms-since-epoch nonce. Same algorithm as the Go
    /// reference: ensure strictly increasing across concurrent calls.
    pub fn next_nonce(&self) -> u64 {
        loop {
            let now = chrono::Utc::now().timestamp_millis();
            let old = self.nonce.load(Ordering::SeqCst);
            let next = if now <= old { old + 1 } else { now };
            if self
                .nonce
                .compare_exchange(old, next, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return next as u64;
            }
        }
    }

    pub async fn place_order(
        &self,
        action: &OrderAction,
        signer: &AgentSigner,
    ) -> Result<OrderResult, ExchangeError> {
        let resp = self.post_action(action, signer, None).await?;
        parse_order_result(&resp)
    }

    pub async fn cancel(
        &self,
        action: &CancelAction,
        signer: &AgentSigner,
    ) -> Result<CancelResult, ExchangeError> {
        let resp = self.post_action(action, signer, None).await?;
        parse_cancel_result(&resp)
    }

    pub async fn cancel_by_cloid(
        &self,
        action: &CancelByCloidAction,
        signer: &AgentSigner,
    ) -> Result<CancelResult, ExchangeError> {
        let resp = self.post_action(action, signer, None).await?;
        parse_cancel_result(&resp)
    }

    pub async fn update_leverage(
        &self,
        action: &UpdateLeverageAction,
        signer: &AgentSigner,
    ) -> Result<(), ExchangeError> {
        let _ = self.post_action(action, signer, None).await?;
        Ok(())
    }

    /// HLP + builder-vault deposit / withdrawal. L1-signed. Returns
    /// `Ok(())` if HL accepted the action — the funds movement is
    /// settled by HL's clearinghouse async; the FE polls vault state.
    pub async fn vault_transfer(
        &self,
        action: &VaultTransferAction,
        signer: &AgentSigner,
    ) -> Result<(), ExchangeError> {
        let _ = self.post_action(action, signer, None).await?;
        Ok(())
    }

    /// Approve an API agent — signed by the USER's main wallet, not by
    /// the agent. The signature is produced FE-side (the user's wallet
    /// extension prompts a typed-data signature). This function only
    /// POSTs the pre-signed envelope to `/exchange`. The signer
    /// argument is `None`: HL expects an external `signature` field
    /// here, not a server-derived one.
    pub async fn submit_approve_agent(
        &self,
        action: &ApproveAgentAction,
        external_signature: Signature,
    ) -> Result<ApprovalResult, ExchangeError> {
        let req = ExchangeRequest {
            action,
            nonce: action.nonce,
            signature: &external_signature,
            vault_address: None,
        };
        let resp = self.post_raw(&req).await?;
        Ok(ApprovalResult {
            status: resp.status,
        })
    }

    async fn post_action<A: serde::Serialize>(
        &self,
        action: &A,
        signer: &AgentSigner,
        vault_address: Option<&str>,
    ) -> Result<ExchangeResponse, ExchangeError> {
        let nonce = self.next_nonce();
        let sig = signer.sign_l1_action(action, nonce, vault_address.unwrap_or(""))?;
        let req = ExchangeRequest {
            action,
            nonce,
            signature: &sig,
            vault_address,
        };
        self.post_raw(&req).await
    }

    async fn post_raw<T: serde::Serialize>(
        &self,
        body: &T,
    ) -> Result<ExchangeResponse, ExchangeError> {
        let resp = self.http.post(&self.base).json(body).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(ExchangeError::Status(
                status.as_u16(),
                truncate(&body, 1024),
            ));
        }
        let env: ExchangeResponse = serde_json::from_slice(&bytes).map_err(|e| {
            ExchangeError::Decode(format!(
                "{e} body={}",
                truncate(&String::from_utf8_lossy(&bytes), 512)
            ))
        })?;
        if env.status != "ok" {
            // HL puts the error in `response` (as a string) when
            // status != "ok".
            let msg = env
                .response
                .as_ref()
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| env.status.clone());
            return Err(ExchangeError::Api(msg));
        }
        Ok(env)
    }
}

fn parse_order_result(env: &ExchangeResponse) -> Result<OrderResult, ExchangeError> {
    // Shape: { response: { type, data: { statuses: [ {resting:{oid}}|{filled:{...}}|"error" ] } } }
    let resp = env
        .response
        .as_ref()
        .ok_or_else(|| ExchangeError::Decode("missing response".into()))?;
    let statuses = resp
        .get("data")
        .and_then(|d| d.get("statuses"))
        .and_then(|s| s.as_array())
        .ok_or_else(|| ExchangeError::Decode("missing data.statuses".into()))?;

    let mut out = Vec::with_capacity(statuses.len());
    for s in statuses {
        if let Some(err) = s.as_str() {
            out.push(OrderStatusEntry::Error(err.to_string()));
            continue;
        }
        if let Some(r) = s.get("resting") {
            if let Some(oid) = r.get("oid").and_then(|x| x.as_u64()) {
                out.push(OrderStatusEntry::Resting { oid });
                continue;
            }
        }
        if let Some(f) = s.get("filled") {
            let oid = f.get("oid").and_then(|x| x.as_u64()).unwrap_or(0);
            let total_sz = f
                .get("totalSz")
                .and_then(|x| x.as_str())
                .unwrap_or("0")
                .to_string();
            let avg_px = f
                .get("avgPx")
                .and_then(|x| x.as_str())
                .unwrap_or("0")
                .to_string();
            out.push(OrderStatusEntry::Filled {
                oid,
                total_sz,
                avg_px,
            });
            continue;
        }
        if let Some(err) = s.get("error").and_then(|x| x.as_str()) {
            out.push(OrderStatusEntry::Error(err.to_string()));
            continue;
        }
        out.push(OrderStatusEntry::Error(format!("unknown status: {s}")));
    }
    Ok(OrderResult { statuses: out })
}

fn parse_cancel_result(env: &ExchangeResponse) -> Result<CancelResult, ExchangeError> {
    // Cancel response shape varies; we just stringify each status row.
    let mut out = Vec::new();
    if let Some(resp) = env.response.as_ref() {
        if let Some(arr) = resp
            .get("data")
            .and_then(|d| d.get("statuses"))
            .and_then(|s| s.as_array())
        {
            for s in arr {
                out.push(s.to_string());
            }
        }
    }
    Ok(CancelResult { statuses: out })
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…(+{} more bytes)", &s[..n], s.len() - n)
    }
}

// Manual Debug for OrderStatusEntry — we use it in tests but want a
// stable representation that includes the variant name.
impl std::fmt::Debug for OrderStatusEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrderStatusEntry::Resting { oid } => write!(f, "Resting(oid={oid})"),
            OrderStatusEntry::Filled {
                oid,
                total_sz,
                avg_px,
            } => {
                write!(f, "Filled(oid={oid}, sz={total_sz}, px={avg_px})")
            }
            OrderStatusEntry::Error(s) => write!(f, "Error({s})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_nonce_is_strictly_monotonic() {
        let c = ExchangeClient::new(Network::Testnet).unwrap();
        let n1 = c.next_nonce();
        let n2 = c.next_nonce();
        let n3 = c.next_nonce();
        assert!(n2 > n1);
        assert!(n3 > n2);
    }

    #[test]
    fn parse_order_result_handles_resting_and_filled() {
        let env = ExchangeResponse {
            status: "ok".into(),
            response: Some(serde_json::json!({
                "type": "order",
                "data": {"statuses": [
                    {"resting": {"oid": 12345}},
                    {"filled": {"oid": 67890, "totalSz": "0.5", "avgPx": "60123.5"}},
                    "tooSmall",
                ]},
            })),
        };
        let r = parse_order_result(&env).unwrap();
        assert_eq!(r.statuses.len(), 3);
        match &r.statuses[0] {
            OrderStatusEntry::Resting { oid } => assert_eq!(*oid, 12345),
            o => panic!("expected resting, got {o:?}"),
        }
        match &r.statuses[1] {
            OrderStatusEntry::Filled {
                oid,
                total_sz,
                avg_px,
            } => {
                assert_eq!(*oid, 67890);
                assert_eq!(total_sz, "0.5");
                assert_eq!(avg_px, "60123.5");
            }
            o => panic!("expected filled, got {o:?}"),
        }
        match &r.statuses[2] {
            OrderStatusEntry::Error(s) => assert_eq!(s, "tooSmall"),
            o => panic!("expected error, got {o:?}"),
        }
    }

    #[test]
    fn parse_order_result_rejects_missing_data() {
        let env = ExchangeResponse {
            status: "ok".into(),
            response: Some(serde_json::json!({"type": "order"})),
        };
        assert!(parse_order_result(&env).is_err());
    }
}
