//! Minimal Solana JSON-RPC client.
//!
//! The signer needs two RPC calls that the simulator alone can't
//! provide: `getAccountInfo` (to read live PumpFun BondingCurve state
//! when deciding the swap route) and `getLatestBlockhash` (to stamp
//! the v0 message before signing). Everything else still goes through
//! `simulator::Simulator` (simulateTransaction) or `relay::RelayClient`
//! (gateway-submit).
//!
//! Surface kept narrow on purpose — no fan-out helpers, no account
//! subscription, no batched reads. The signer fetches what it needs
//! at handle-one time and moves on.

use base64::Engine as _;
use serde::Deserialize;
use solana_sdk::{hash::Hash, pubkey::Pubkey};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("rpc returned status {0}: {1}")]
    Status(u16, String),
    #[error("rpc error: {0}")]
    RpcError(String),
    #[error("decode: {0}")]
    Decode(String),
}

/// URL of the gateway's token-gated Solana JSON-RPC proxy (v0.3.0
/// slice 10) — the signer's ZERO-CONFIG default when the user has not
/// set an RPC override and no `SOLANA_RPC_URL` is exported. The auth
/// token rides as a query param, the same pattern as the gateway WS
/// multiplexer's `/ws?token=` (the JWT is the user's own, the URL
/// never leaves the device except toward our own gateway).
pub fn gateway_proxy_rpc_url(gateway_base: &str, token: &str) -> String {
    let token_enc = url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>();
    format!(
        "{}/api/rpc/solana?token={}",
        gateway_base.trim_end_matches('/'),
        token_enc
    )
}

#[derive(Clone)]
pub struct RpcClient {
    http: reqwest::Client,
    url: String,
}

impl RpcClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            url: url.into(),
        }
    }

    /// `getAccountInfo` with base64 encoding. Returns `Ok(None)` when
    /// the account does not exist (RPC returns `value: null`).
    pub async fn get_account_data(&self, pubkey: &Pubkey) -> Result<Option<Vec<u8>>, RpcError> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getAccountInfo",
            "params": [
                pubkey.to_string(),
                { "encoding": "base64", "commitment": "processed" }
            ]
        });
        let resp = self.http.post(&self.url).json(&req).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RpcError::Status(status.as_u16(), body));
        }
        let parsed: AccountInfoResp = resp
            .json()
            .await
            .map_err(|e| RpcError::Decode(e.to_string()))?;
        if let Some(e) = parsed.error {
            return Err(RpcError::RpcError(format!(
                "code={} msg={}",
                e.code, e.message
            )));
        }
        let result = parsed
            .result
            .ok_or_else(|| RpcError::Decode("missing result".into()))?;
        match result.value {
            None => Ok(None),
            Some(v) => decode_account_data(&v.data),
        }
    }

    /// `getAccountInfo` returning only the account's OWNER program id.
    /// Used to resolve a mint's token program (legacy SPL Token vs
    /// Token-2022) before deriving ATAs — pump launches Token-2022
    /// mints since 2025 (audit M2). Uses a zero-length `dataSlice` so
    /// the (potentially large) account data never crosses the wire.
    /// Returns `Ok(None)` when the account does not exist.
    pub async fn get_account_owner(&self, pubkey: &Pubkey) -> Result<Option<Pubkey>, RpcError> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getAccountInfo",
            "params": [
                pubkey.to_string(),
                {
                    "encoding": "base64",
                    "commitment": "processed",
                    "dataSlice": { "offset": 0, "length": 0 }
                }
            ]
        });
        let resp = self.http.post(&self.url).json(&req).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RpcError::Status(status.as_u16(), body));
        }
        let parsed: AccountInfoResp = resp
            .json()
            .await
            .map_err(|e| RpcError::Decode(e.to_string()))?;
        if let Some(e) = parsed.error {
            return Err(RpcError::RpcError(format!(
                "code={} msg={}",
                e.code, e.message
            )));
        }
        let result = parsed
            .result
            .ok_or_else(|| RpcError::Decode("missing result".into()))?;
        match result.value {
            None => Ok(None),
            Some(v) => {
                let owner = v
                    .owner
                    .ok_or_else(|| RpcError::Decode("missing owner".into()))?;
                Pubkey::from_str(&owner)
                    .map(Some)
                    .map_err(|e| RpcError::Decode(e.to_string()))
            }
        }
    }

    /// `getLatestBlockhash`. Returns the parsed `Hash` ready to drop
    /// into a `v0::Message::try_compile` call.
    pub async fn get_latest_blockhash(&self) -> Result<Hash, RpcError> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getLatestBlockhash",
            "params": [{ "commitment": "processed" }]
        });
        let resp = self.http.post(&self.url).json(&req).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RpcError::Status(status.as_u16(), body));
        }
        let parsed: BlockhashResp = resp
            .json()
            .await
            .map_err(|e| RpcError::Decode(e.to_string()))?;
        if let Some(e) = parsed.error {
            return Err(RpcError::RpcError(format!(
                "code={} msg={}",
                e.code, e.message
            )));
        }
        let v = parsed
            .result
            .ok_or_else(|| RpcError::Decode("missing result".into()))?
            .value;
        Hash::from_str(&v.blockhash).map_err(|e| RpcError::Decode(e.to_string()))
    }

    /// `getBalance` — the wallet's native SOL balance in lamports.
    /// Used by the copy-trade loop to resolve pct-of-balance sizing
    /// right before a buy.
    pub async fn get_balance(&self, pubkey: &Pubkey) -> Result<u64, RpcError> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBalance",
            "params": [pubkey.to_string(), { "commitment": "processed" }]
        });
        let resp = self.http.post(&self.url).json(&req).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RpcError::Status(status.as_u16(), body));
        }
        let parsed: BalanceResp = resp
            .json()
            .await
            .map_err(|e| RpcError::Decode(e.to_string()))?;
        if let Some(e) = parsed.error {
            return Err(RpcError::RpcError(format!(
                "code={} msg={}",
                e.code, e.message
            )));
        }
        parsed
            .result
            .map(|r| r.value)
            .ok_or_else(|| RpcError::Decode("missing result".into()))
    }

    /// `getTokenAccountBalance` for the given SPL token account. Returns
    /// the **raw base-unit balance** (NOT ui-decimals) so PumpSwap pool
    /// vault reads round-trip cleanly into the `u64` reserves the
    /// quote math expects.
    ///
    /// Returns `Ok(None)` when the account doesn't exist or doesn't
    /// hold a token-program payload (Solana returns an error in that
    /// case, which we map to `None` so the caller can treat
    /// missing-pool as a routable signal).
    pub async fn get_token_account_balance(
        &self,
        pubkey: &Pubkey,
    ) -> Result<Option<u64>, RpcError> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getTokenAccountBalance",
            "params": [pubkey.to_string(), { "commitment": "processed" }]
        });
        let resp = self.http.post(&self.url).json(&req).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RpcError::Status(status.as_u16(), body));
        }
        let parsed: TokenBalanceResp = resp
            .json()
            .await
            .map_err(|e| RpcError::Decode(e.to_string()))?;
        if let Some(e) = parsed.error {
            // Map "account does not exist / invalid" to None.
            // Solana's error codes for this are -32602 (invalid
            // params) / -32004 (not found) — we don't try to
            // distinguish, anything error-shaped becomes None on
            // this happy-path query.
            if e.code <= -32000 {
                return Ok(None);
            }
            return Err(RpcError::RpcError(format!(
                "code={} msg={}",
                e.code, e.message
            )));
        }
        let amount_str = parsed
            .result
            .and_then(|r| r.value.amount)
            .ok_or_else(|| RpcError::Decode("missing value.amount".into()))?;
        amount_str
            .parse::<u64>()
            .map(Some)
            .map_err(|e| RpcError::Decode(e.to_string()))
    }
}

/// Pulled out so unit tests can exercise the decode path without
/// having to spin a mock HTTP server.
fn decode_account_data(data: &[String]) -> Result<Option<Vec<u8>>, RpcError> {
    // Solana RPC returns `data: [base64String, "base64"]` for the
    // base64 encoding. Validate shape + encoding label.
    if data.len() != 2 {
        return Err(RpcError::Decode(format!(
            "expected [data, encoding] tuple, got {} entries",
            data.len()
        )));
    }
    if data[1] != "base64" {
        return Err(RpcError::Decode(format!(
            "unexpected encoding {}, expected base64",
            data[1]
        )));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&data[0])
        .map_err(|e| RpcError::Decode(e.to_string()))?;
    Ok(Some(bytes))
}

// ─── wire types ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct AccountInfoResp {
    #[allow(dead_code)]
    jsonrpc: String,
    result: Option<AccountInfoResult>,
    error: Option<RpcErrorBody>,
}

#[derive(Debug, Deserialize)]
struct AccountInfoResult {
    #[allow(dead_code)]
    context: Option<serde_json::Value>,
    value: Option<AccountInfoValue>,
}

#[derive(Debug, Deserialize)]
struct AccountInfoValue {
    /// `[base64String, "base64"]` per Solana RPC.
    data: Vec<String>,
    /// Owner program id (base58). Present in every live RPC response;
    /// optional here so `get_account_data` (which ignores it) keeps
    /// parsing fixtures without it.
    #[serde(default)]
    owner: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BlockhashResp {
    #[allow(dead_code)]
    jsonrpc: String,
    result: Option<BlockhashResult>,
    error: Option<RpcErrorBody>,
}

#[derive(Debug, Deserialize)]
struct BlockhashResult {
    #[allow(dead_code)]
    context: Option<serde_json::Value>,
    value: BlockhashValue,
}

#[derive(Debug, Deserialize)]
struct BlockhashValue {
    blockhash: String,
    #[allow(dead_code)]
    #[serde(rename = "lastValidBlockHeight")]
    last_valid_block_height: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RpcErrorBody {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct BalanceResp {
    #[allow(dead_code)]
    jsonrpc: String,
    result: Option<BalanceResult>,
    error: Option<RpcErrorBody>,
}

#[derive(Debug, Deserialize)]
struct BalanceResult {
    #[allow(dead_code)]
    context: Option<serde_json::Value>,
    value: u64,
}

#[derive(Debug, Deserialize)]
struct TokenBalanceResp {
    #[allow(dead_code)]
    jsonrpc: String,
    result: Option<TokenBalanceResult>,
    error: Option<RpcErrorBody>,
}

#[derive(Debug, Deserialize)]
struct TokenBalanceResult {
    #[allow(dead_code)]
    context: Option<serde_json::Value>,
    value: TokenBalanceValue,
}

#[derive(Debug, Deserialize)]
struct TokenBalanceValue {
    /// Raw u64 as string ("12345"). Parsed lazily so a malformed
    /// payload surfaces as `RpcError::Decode` rather than panicking.
    amount: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_proxy_url_shape_and_encoding() {
        let u = gateway_proxy_rpc_url("https://api-v2.degenbox.app", "ey.ab-c_d");
        assert_eq!(
            u,
            "https://api-v2.degenbox.app/api/rpc/solana?token=ey.ab-c_d"
        );
        // Trailing slash trimmed; reserved chars percent-encoded.
        let u = gateway_proxy_rpc_url("http://localhost:8090/", "a+b&c");
        assert_eq!(u, "http://localhost:8090/api/rpc/solana?token=a%2Bb%26c");
    }

    #[test]
    fn decode_account_data_happy_path() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4]);
        let out = decode_account_data(&[encoded, "base64".into()]).unwrap();
        assert_eq!(out, Some(vec![1, 2, 3, 4]));
    }

    #[test]
    fn decode_account_data_rejects_wrong_encoding() {
        let encoded = base64::engine::general_purpose::STANDARD.encode([1u8]);
        let r = decode_account_data(&[encoded, "base58".into()]);
        assert!(matches!(r, Err(RpcError::Decode(_))));
    }

    #[test]
    fn decode_account_data_rejects_bad_shape() {
        let r = decode_account_data(&["a".into()]);
        assert!(matches!(r, Err(RpcError::Decode(_))));
    }

    #[test]
    fn decode_account_data_rejects_bad_base64() {
        let r = decode_account_data(&["!!!".into(), "base64".into()]);
        assert!(matches!(r, Err(RpcError::Decode(_))));
    }

    /// Confirm we can parse the JSON shape the real Solana RPC returns
    /// (captured from a `getAccountInfo` against a system program PDA).
    #[test]
    fn account_info_resp_deserialises() {
        let raw = r#"{
            "jsonrpc":"2.0",
            "result":{
                "context":{"slot":12345},
                "value":{
                    "data":["AQID","base64"],
                    "executable":false,
                    "lamports":1,
                    "owner":"11111111111111111111111111111111",
                    "rentEpoch":0
                }
            },
            "id":1
        }"#;
        let parsed: AccountInfoResp = serde_json::from_str(raw).unwrap();
        let value = parsed.result.unwrap().value.unwrap();
        assert_eq!(value.data, vec!["AQID".to_string(), "base64".to_string()]);
    }

    #[test]
    fn account_info_resp_handles_null_value() {
        let raw = r#"{
            "jsonrpc":"2.0",
            "result":{"context":{"slot":1},"value":null},
            "id":1
        }"#;
        let parsed: AccountInfoResp = serde_json::from_str(raw).unwrap();
        assert!(parsed.result.unwrap().value.is_none());
    }

    #[test]
    fn blockhash_resp_deserialises() {
        let raw = r#"{
            "jsonrpc":"2.0",
            "result":{
                "context":{"slot":12345},
                "value":{
                    "blockhash":"GH7ome3EiwEr7tu9JuTh2dpYWBJK3z69Xm1ZE3MEE6JC",
                    "lastValidBlockHeight":12345
                }
            },
            "id":1
        }"#;
        let parsed: BlockhashResp = serde_json::from_str(raw).unwrap();
        let v = parsed.result.unwrap().value;
        assert_eq!(v.blockhash, "GH7ome3EiwEr7tu9JuTh2dpYWBJK3z69Xm1ZE3MEE6JC");
        // Hash::from_str should parse it.
        Hash::from_str(&v.blockhash).expect("blockhash parses");
    }

    #[test]
    fn balance_resp_deserialises() {
        let raw = r#"{
            "jsonrpc":"2.0",
            "result":{"context":{"slot":1},"value":123456789},
            "id":1
        }"#;
        let parsed: BalanceResp = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.result.unwrap().value, 123_456_789);
    }

    #[test]
    fn rpc_error_body_deserialises() {
        let raw = r#"{
            "jsonrpc":"2.0",
            "error":{"code":-32602,"message":"Invalid params"},
            "id":1
        }"#;
        let parsed: AccountInfoResp = serde_json::from_str(raw).unwrap();
        let err = parsed.error.unwrap();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "Invalid params");
    }
}
