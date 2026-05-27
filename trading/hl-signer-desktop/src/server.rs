//! Thin client for the DegenBox server's signer-facing routes.
//!
//! Endpoints used:
//!
//!   - `POST /api/hyperliquid/exchange/signer/register`
//!   - `GET  /api/hyperliquid/exchange/instructions/pending`
//!   - `POST /api/hyperliquid/exchange/order/result`
//!
//! All requests carry an `Authorization: Bearer <api_token>` header.

use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server returned {0}: {1}")]
    Status(StatusCode, String),
    #[error("api token missing — run `hl-signer-desktop register` or set in config")]
    NoToken,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterReq {
    pub agent_address: String,
    pub client_version: Option<String>,
    pub host_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RegisterResp {
    pub user_id: String,
    pub agent_address: String,
    pub registered_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PendingRow {
    pub id: String,
    pub cloid: String,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResultReq {
    pub cloid: String,
    pub oid: Option<i64>,
    pub status: String,
    pub filled_size_usd: Option<String>,
    pub err_msg: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RedeemRegistrationReq {
    pub token: String,
    pub agent_address: String,
    pub client_version: Option<String>,
    pub host_id: Option<String>,
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

    /// Redeem a one-shot onboarding registration token. No JWT/api-token
    /// auth — the token IS the auth.
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
            return Err(ServerError::Status(status, text));
        }
        let parsed: RegisterResp = serde_json::from_str(&text)
            .map_err(|e| ServerError::Status(status, format!("decode: {e}")))?;
        Ok(parsed)
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
            return Err(ServerError::Status(status, text));
        }
        let parsed: RegisterResp = serde_json::from_str(&text)
            .map_err(|e| ServerError::Status(status, format!("decode: {e}")))?;
        Ok(parsed)
    }

    pub async fn pending(
        &self,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<PendingRow>, ServerError> {
        let url = format!(
            "{}/api/hyperliquid/exchange/instructions/pending",
            self.base
        );
        let mut req = self.http.get(&url).bearer_auth(&self.token);
        let mut params: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(s) = since {
            params.push(("since", s.to_rfc3339()));
        }
        req = req.query(&params);
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ServerError::Status(status, text));
        }
        let rows: Vec<PendingRow> = serde_json::from_str(&text)
            .map_err(|e| ServerError::Status(status, format!("decode: {e}")))?;
        Ok(rows)
    }

    pub async fn post_result(&self, body: &ResultReq) -> Result<(), ServerError> {
        let url = format!("{}/api/hyperliquid/exchange/order/result", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ServerError::Status(status, text));
        }
        Ok(())
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
}
