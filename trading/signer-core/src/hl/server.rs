//! Thin client for the DegenBox gateway's signer-facing HL routes.
//!
//! Canonical merge of `hl-signer-desktop/src/server.rs` (the proven CLI
//! implementation) and the signer-app port — wire shape byte-identical
//! end-to-end.
//! Endpoints (all under `/api/hyperliquid/exchange/`):
//!
//!   - `POST signer/redeem-registration`  (one-shot onboarding token)
//!   - `POST signer/register`             (long-lived bearer heartbeat)
//!   - `GET  instructions/pending`        (claim-on-read poll)
//!   - `POST order/result`                (report outcome)
//!   - `POST signer/verify-totp`          (per-trade 2FA bypass token)

use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server returned {0}: {1}")]
    Status(StatusCode, String),
    #[error("api token missing — register the signer first")]
    NoToken,
}

fn truncate_body(s: &str) -> String {
    let s = s.trim();
    if s.starts_with("<html") || s.starts_with("<!DOCTYPE html>") {
        return "<html body omitted>".into();
    }
    if s.len() > 150 {
        return format!("{}...", &s[..150]);
    }
    s.to_string()
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterReq {
    pub agent_address: String,
    pub client_version: Option<String>,
    pub host_id: Option<String>,
    /// The user's HL MASTER wallet this signer pairs with. The gateway
    /// only delivers instructions to approved rows that declare this —
    /// a bearer registration without it lands approved-but-unpaired and
    /// every submit 403s. Skipped on the wire when absent so old
    /// gateways (and old request shapes) stay compatible; the gateway
    /// coalesce-preserves an existing pairing when the field is omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired_with_account: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterResp {
    pub user_id: String,
    pub agent_address: String,
    #[allow(dead_code)]
    pub registered_at: DateTime<Utc>,
    /// Signer JWT minted by `redeem-registration` (absent on the legacy
    /// bearer path where the caller already holds a token).
    #[serde(default)]
    pub api_token: Option<String>,
    #[serde(default)]
    pub discord_handle: Option<String>,
}

/// `GET /signer/pairing` response — deliberately lenient (everything
/// but `state` defaulted) so additive gateway fields never break a
/// deployed client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PairingStatus {
    /// `not_registered | revoked | pending_approval | unpaired |
    /// wallet_mismatch | paired_offline | paired_live`
    pub state: String,
    #[serde(default)]
    pub linked_address: Option<String>,
    #[serde(default)]
    pub paired_with_account: Option<String>,
    #[serde(default)]
    pub agent_address: Option<String>,
    #[serde(default)]
    pub last_heartbeat_at: Option<String>,
    #[serde(default)]
    pub live: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PendingRow {
    #[allow(dead_code)]
    pub id: String,
    pub cloid: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
    /// HL MASTER wallet this instruction must execute on (lowercase
    /// 0x…). Stamped by multi-client gateways; `None` on rows from
    /// legacy gateways/producers — `serde(default)` keeps old and new
    /// wire shapes mutually compatible.
    #[serde(default)]
    pub target_wallet: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResultReq {
    pub cloid: String,
    pub oid: Option<i64>,
    pub status: String,
    pub filled_size_usd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_pnl: Option<String>,
    pub err_msg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub posted_to_hl_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TotpChallenge {
    pub challenge_id: String,
    #[allow(dead_code)]
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyTotpReq {
    pub challenge_id: String,
    pub code: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VerifyTotpResp {
    pub bypass_token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemRegistrationReq {
    pub token: String,
    pub agent_address: String,
    pub client_version: Option<String>,
    pub host_id: Option<String>,
    /// The user's HL MASTER wallet (`0x…`). REQUIRED for trade delivery:
    /// the gateway refuses to hand out instructions unless the heartbeat
    /// row has `paired_with_account IS NOT NULL`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paired_with_account: Option<String>,
    /// 6-digit TOTP code, attached on the retried POST after a 428.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totp_code: Option<String>,
}

/// Query params for `GET /instructions/pending`. Pure — unit-tested so
/// the wallet-scoping contract (`wallet` present ⇔ scoped claim) is
/// pinned at the wire level.
fn pending_params(
    since: Option<DateTime<Utc>>,
    limit: i64,
    wallet: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut params: Vec<(&'static str, String)> = vec![("limit", limit.to_string())];
    if let Some(s) = since {
        params.push(("since", s.to_rfc3339()));
    }
    if let Some(w) = wallet.map(str::trim).filter(|w| !w.is_empty()) {
        params.push(("wallet", w.to_ascii_lowercase()));
    }
    params
}

#[derive(Clone, Debug)]
pub struct ServerClient {
    http: Client,
    base: String,
    token: String,
}

impl ServerClient {
    pub fn new(base: String, token: String) -> Result<Self, ServerError> {
        if token.is_empty() {
            return Err(ServerError::NoToken);
        }
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token,
        })
    }

    /// Redeem a one-shot onboarding registration token. No bearer auth —
    /// the token IS the auth.
    pub async fn redeem_registration(
        base: &str,
        body: &RedeemRegistrationReq,
    ) -> Result<RegisterResp, ServerError> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        let url = format!(
            "{}/api/hyperliquid/exchange/signer/redeem-registration",
            base.trim_end_matches('/')
        );
        let resp = http.post(&url).json(body).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ServerError::Status(status, truncate_body(&text)));
        }
        serde_json::from_str(&text).map_err(|e| ServerError::Status(status, format!("decode: {e}")))
    }

    pub async fn register(&self, body: &RegisterReq) -> Result<RegisterResp, ServerError> {
        let url = format!("{}/api/hyperliquid/exchange/signer/register", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ServerError::Status(status, truncate_body(&text)));
        }
        serde_json::from_str(&text).map_err(|e| ServerError::Status(status, format!("decode: {e}")))
    }

    /// Server-side pairing truth for this signer
    /// (`GET /signer/pairing`). Lets a client refuse to claim "paired"
    /// in its own UI when the gateway disagrees (wallet_mismatch,
    /// unpaired, revoked, …).
    pub async fn pairing(&self) -> Result<PairingStatus, ServerError> {
        let url = format!("{}/api/hyperliquid/exchange/signer/pairing", self.base);
        let resp = self.http.get(&url).bearer_auth(&self.token).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ServerError::Status(status, truncate_body(&text)));
        }
        serde_json::from_str(&text).map_err(|e| ServerError::Status(status, format!("decode: {e}")))
    }

    pub async fn pending(
        &self,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<PendingRow>, ServerError> {
        self.pending_scoped(since, limit, None).await
    }

    /// Claim pending instructions, optionally scoped to one HL MASTER
    /// wallet (`?wallet=0x…`). Multi-client gateways then claim ONLY
    /// rows stamped `target_wallet = lower(wallet)`; `None` keeps the
    /// legacy user-scoped claim (everything, incl. unstamped rows).
    /// Old gateways ignore the unknown query param — pair a scoped poll
    /// with the [`crate::hl::daemon::ClaimScope`] belt so an ignored
    /// filter can never execute another wallet's work.
    pub async fn pending_scoped(
        &self,
        since: Option<DateTime<Utc>>,
        limit: i64,
        wallet: Option<&str>,
    ) -> Result<Vec<PendingRow>, ServerError> {
        let url = format!(
            "{}/api/hyperliquid/exchange/instructions/pending",
            self.base
        );
        let params = pending_params(since, limit, wallet);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .query(&params)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ServerError::Status(status, truncate_body(&text)));
        }
        serde_json::from_str(&text).map_err(|e| ServerError::Status(status, format!("decode: {e}")))
    }

    pub async fn post_result(&self, body: &ResultReq) -> Result<(), ServerError> {
        self.post_result_inner(body, None).await
    }

    pub async fn post_result_with_bypass(
        &self,
        body: &ResultReq,
        bypass_token: &str,
    ) -> Result<(), ServerError> {
        self.post_result_inner(body, Some(bypass_token)).await
    }

    async fn post_result_inner(
        &self,
        body: &ResultReq,
        bypass_token: Option<&str>,
    ) -> Result<(), ServerError> {
        let url = format!("{}/api/hyperliquid/exchange/order/result", self.base);
        let mut builder = self.http.post(&url).bearer_auth(&self.token).json(body);
        if let Some(tok) = bypass_token {
            builder = builder.header("X-Totp-Bypass", tok);
        }
        let resp = builder.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ServerError::Status(status, truncate_body(&text)));
        }
        Ok(())
    }

    /// Parse the HTTP 428 body into a `TotpChallenge`, or `None` when the
    /// body doesn't carry the `totp_required` shape we understand.
    pub fn parse_totp_challenge(body: &str) -> Option<TotpChallenge> {
        #[derive(Deserialize)]
        struct Raw {
            reason: String,
            challenge_id: String,
            expires_at: String,
        }
        let raw: Raw = serde_json::from_str(body).ok()?;
        if raw.reason != "totp_required" {
            warn!(reason = %raw.reason, "unexpected 428 reason, ignoring");
            return None;
        }
        Some(TotpChallenge {
            challenge_id: raw.challenge_id,
            expires_at: raw.expires_at,
        })
    }

    pub async fn verify_totp(&self, challenge_id: &str, code: &str) -> Result<String, ServerError> {
        let url = format!("{}/api/hyperliquid/exchange/signer/verify-totp", self.base);
        let body = VerifyTotpReq {
            challenge_id: challenge_id.to_string(),
            code: code.to_string(),
        };
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ServerError::Status(status, truncate_body(&text)));
        }
        let parsed: VerifyTotpResp = serde_json::from_str(&text)
            .map_err(|e| ServerError::Status(status, format!("decode: {e}")))?;
        Ok(parsed.bypass_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_token_is_rejected_at_construction() {
        let err = ServerClient::new("https://x".into(), "".into()).unwrap_err();
        assert!(matches!(err, ServerError::NoToken));
    }

    #[test]
    fn trailing_slash_in_base_is_stripped() {
        let c = ServerClient::new("https://x/".into(), "t".into()).unwrap();
        assert_eq!(c.base, "https://x");
    }

    #[test]
    fn totp_challenge_parsed_from_428_response() {
        let body = r#"{"reason":"totp_required","challenge_id":"abc123","expires_at":"2026-05-28T12:00:00Z"}"#;
        let c = ServerClient::parse_totp_challenge(body).expect("should parse");
        assert_eq!(c.challenge_id, "abc123");
    }

    #[test]
    fn totp_challenge_ignores_wrong_reason() {
        let body = r#"{"reason":"rate_limited","challenge_id":"x","expires_at":"y"}"#;
        assert!(ServerClient::parse_totp_challenge(body).is_none());
    }

    #[test]
    fn totp_challenge_handles_malformed_body() {
        assert!(ServerClient::parse_totp_challenge("not json").is_none());
        assert!(ServerClient::parse_totp_challenge("{}").is_none());
    }

    #[test]
    fn pending_params_carry_wallet_only_when_scoped() {
        // Legacy (unscoped) poll: NO wallet param on the wire — the
        // gateway then claims everything for the user (CLI behaviour).
        let p = pending_params(None, 20, None);
        assert!(p.iter().all(|(k, _)| *k != "wallet"));
        // Scoped poll: wallet present, lowercased (the gateway compares
        // `target_wallet = lower($4)`).
        let p = pending_params(None, 20, Some("0xAbC123"));
        assert_eq!(
            p.iter()
                .find(|(k, _)| *k == "wallet")
                .map(|(_, v)| v.as_str()),
            Some("0xabc123")
        );
        // Whitespace/empty wallets degrade to the unscoped poll instead
        // of sending a filter that matches nothing.
        let p = pending_params(None, 20, Some("   "));
        assert!(p.iter().all(|(k, _)| *k != "wallet"));
    }

    #[test]
    fn pending_row_parses_with_and_without_target_wallet() {
        // New gateways stamp `target_wallet`; rows from old gateways
        // don't carry the field at all. Both must decode.
        let stamped = serde_json::json!({
            "id": "i1", "cloid": "c1", "payload": {"kind": "order"},
            "created_at": "2026-06-10T12:00:00Z",
            "target_wallet": "0xabc",
        });
        let row: PendingRow = serde_json::from_value(stamped).unwrap();
        assert_eq!(row.target_wallet.as_deref(), Some("0xabc"));
        let legacy = serde_json::json!({
            "id": "i2", "cloid": "c2", "payload": {},
            "created_at": "2026-06-10T12:00:00Z",
        });
        let row: PendingRow = serde_json::from_value(legacy).unwrap();
        assert_eq!(row.target_wallet, None);
    }
}
