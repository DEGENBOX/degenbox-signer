//! Gateway-relay client.
//!
//! Talks to the DegenBox gateway's `/api/trading/intents/*` endpoints
//! to create + submit a trade. The gateway is the only entity that
//! gets to fan-out to Falcon QUIC + Jito (multi-region race) — we
//! relay our signed bytes through it instead of broadcasting to
//! Solana RPC directly so the user benefits from the platform's
//! race-submit infrastructure.
//!
//! ## Auth
//!
//! Bearer JWT in the `Authorization` header. Caller is responsible
//! for obtaining one (Discord OAuth or `/auth/dev-login`).

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("gateway responded with status {0}: {1}")]
    Status(u16, String),
    #[error("decode: {0}")]
    Decode(String),
}

#[derive(Clone)]
pub struct RelayClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl RelayClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client"),
            base: base.into(),
            token: token.into(),
        }
    }

    /// `POST /api/trading/intents` — creates a row in `pending`.
    pub async fn create_intent(&self, req: &CreateIntentReq) -> Result<IntentRow, RelayError> {
        let url = format!("{}/api/trading/intents", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RelayError::Status(status.as_u16(), body));
        }
        resp.json::<IntentRow>()
            .await
            .map_err(|e| RelayError::Decode(e.to_string()))
    }

    /// `GET /api/alpha/presets/{preset_id}/matches` — bot-engine
    /// signal source. Returns the recent matches that the server's
    /// filter worker wrote into `alpha_preset_matches`. Polled in a
    /// loop by the CLI's `bot run` command; the extension / desktop
    /// daemon will switch to NATS subscription in T.4.
    pub async fn fetch_preset_matches(
        &self,
        preset_id: &str,
        limit: u32,
    ) -> Result<Vec<PresetMatchRow>, RelayError> {
        let url = format!(
            "{}/api/alpha/presets/{}/matches?limit={}",
            self.base, preset_id, limit
        );
        let resp = self.http.get(&url).bearer_auth(&self.token).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RelayError::Status(status.as_u16(), body));
        }
        resp.json::<Vec<PresetMatchRow>>()
            .await
            .map_err(|e| RelayError::Decode(e.to_string()))
    }

    /// `GET /auth/me` — fetch the authenticated user's claims. The
    /// signer needs `sub` (user-uuid) to subscribe to its own
    /// `trading.sell.needed.{user_id}` subject — that subject is
    /// user-scoped by the WS multiplexer (wildcards rejected). Wraps
    /// JSON decoding so callers don't drag in the full claims type.
    pub async fn fetch_user_id(&self) -> Result<uuid::Uuid, RelayError> {
        let url = format!("{}/auth/me", self.base);
        let resp = self.http.get(&url).bearer_auth(&self.token).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RelayError::Status(status.as_u16(), body));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| RelayError::Decode(e.to_string()))?;
        let sub = body
            .get("sub")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RelayError::Decode("auth/me missing sub".into()))?;
        uuid::Uuid::parse_str(sub).map_err(|e| RelayError::Decode(e.to_string()))
    }

    /// `POST /api/trading/bot/sessions` — register a new bot session
    /// with the gateway so fills can be attributed to it and the
    /// dashboard shows expiry + budget tracking.
    pub async fn create_bot_session(
        &self,
        req: &CreateBotSessionReq,
    ) -> Result<BotSessionRow, RelayError> {
        let url = format!("{}/api/trading/bot/sessions", self.base);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(req)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RelayError::Status(status.as_u16(), body));
        }
        resp.json::<BotSessionRow>()
            .await
            .map_err(|e| RelayError::Decode(e.to_string()))
    }

    /// `DELETE /api/trading/bot/sessions/{id}` — mark the gateway
    /// session cancelled. Best-effort: the daemon calls this when the
    /// user stops the bot so the dashboard reflects the correct status
    /// without waiting for the nightly expiry sweep.
    pub async fn cancel_bot_session(&self, session_id: &str) -> Result<(), RelayError> {
        let url = format!("{}/api/trading/bot/sessions/{}", self.base, session_id);
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RelayError::Status(status.as_u16(), body));
        }
        Ok(())
    }

    /// `POST /api/trading/intents/{id}/submit` — relays the signed
    /// transaction through the gateway's race-submit infrastructure
    /// (Falcon QUIC + Jito multi-region). Returns one or more order
    /// rows (one per submit-path that succeeded).
    pub async fn submit(
        &self,
        intent_id: &str,
        signed_tx_b64: &str,
        submit_mode: Option<&str>,
    ) -> Result<SubmitResp, RelayError> {
        let url = format!("{}/api/trading/intents/{}/submit", self.base, intent_id);
        let body = SubmitReq {
            signed_tx_b64: signed_tx_b64.to_string(),
            submit_mode: submit_mode.map(str::to_string),
        };
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RelayError::Status(status.as_u16(), body));
        }
        resp.json::<SubmitResp>()
            .await
            .map_err(|e| RelayError::Decode(e.to_string()))
    }
}

// ─── wire types ────────────────────────────────────────────────────

/// Mirrors `module-trading::domain::CreateIntentReq`. Re-declared
/// here because signer-core is a separate workspace and we don't
/// want to depend on the backend crate.
#[derive(Debug, Clone, Serialize)]
pub struct CreateIntentReq {
    pub side: String, // "buy" | "sell"
    pub input_mint: String,
    pub output_mint: String,
    pub amount_in_lamports: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slippage_bps: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit_mode: Option<String>, // "falcon" | "falcon_jito" | "max_race"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip_lamports: Option<i64>,
    /// UUID of the preset that produced this signal — backend uses it
    /// for preset-level fill analytics + audit-trail join.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    /// UUID of the active bot session driving this intent. Backend
    /// links the intent to `trading_bot_sessions` so the
    /// `record_spend` path can bump `spent_lamports` once the fill
    /// confirms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_snapshot: Option<serde_json::Value>,
    /// UUID of the `sol_copy_trade_configs` row when this intent is a
    /// copy-trade execution. The backend verifies ownership, uses it to
    /// auto-arm the config's default TP/SL ladder on the buy fill, and
    /// sums tagged intents for the cumulative per-mint position cap.
    /// `None` (omitted on the wire) for every non-copy trade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_config_id: Option<String>,
    /// Client-supplied idempotency token (audit M4). The gateway
    /// dedups intent creation on `(owner, client_token)` within a TTL
    /// — two unlocked devices replaying the same copy/sell event
    /// produce ONE intent instead of two real trades. The engine
    /// stamps a deterministic value derived from the triggering event
    /// id (copy `intent_id` / sell `target_id` / signal `call_id`).
    /// Gateway caps the length at 100 chars; longer tokens are
    /// silently ignored server-side, so keep it short.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IntentRow {
    pub id: String,
    pub status: String,
    pub side: String,
    pub input_mint: String,
    pub output_mint: String,
    pub amount_in_lamports: i64,
    pub slippage_bps: i32,
    pub submit_mode: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct SubmitReq {
    signed_tx_b64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    submit_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubmitResp {
    pub intent_id: String,
    pub orders: Vec<OrderSummary>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrderSummary {
    pub id: String,
    pub signature: String,
    pub path: String,
    pub status: String,
    pub submit_latency_ms: Option<i32>,
    pub error: Option<String>,
}

/// One leg of a default TP/SL ladder attached to a bot session.
/// Mirrors `module-trading::targets::LegSpec`. When a session carries
/// a ladder, the GATEWAY auto-arms it server-side on every confirmed
/// bot BUY fill — the signer never evaluates ladders itself, it only
/// executes the `trading.sell.needed` events the gateway's worker
/// publishes when a leg trips. (That is also why `BotConfig` has no
/// ladder field: the engine's job ends at the buy; protection is a
/// backend concern keyed off the session row.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LadderLegSpec {
    /// `"tp"` or `"sl"`.
    pub kind: String,
    /// Positive percent vs entry, serialised as a string (Decimal on
    /// the gateway side). TP fires at `pct_change >= trigger`, SL at
    /// `pct_change <= -trigger`.
    pub trigger_pct: String,
    /// Fraction of the ladder's anchor amount this leg sells (bps,
    /// 1..=10000).
    pub sell_fraction_bps: i32,
}

/// Request to create a bot session on the gateway. The signer calls
/// this when the user arms the bot engine so the backend can track
/// fill volume, enforce expiry, and show budget progress on the
/// dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct CreateBotSessionReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    pub wallet_pubkey: String,
    pub budget_lamports: i64,
    pub per_trade_lamports: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_token_cap_lamports: Option<i64>,
    pub tip_lamports: i64,
    /// Session expiry — Unix milliseconds. Gateway rejects sessions
    /// that expire in the past or more than 30 days out.
    pub expires_at_unix_ms: i64,
    /// Optional default TP/SL ladder the gateway auto-arms on every
    /// confirmed bot BUY fill. Omitted from the wire when `None` so
    /// pre-ladder gateways accept the request unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_ladder: Option<Vec<LadderLegSpec>>,
}

/// Gateway-side row for a bot session. Subset of
/// `trading_bot_sessions` that the signer needs for status display.
#[derive(Debug, Clone, Deserialize)]
pub struct BotSessionRow {
    pub id: String,
    pub status: String,
    pub budget_lamports: i64,
    pub per_trade_lamports: i64,
    pub spent_lamports: i64,
    pub fill_count: i32,
    pub expires_at: String,
    pub created_at: String,
}

/// Server-side row from `alpha_preset_matches` join. Reshape of what
/// the gateway returns from `/api/alpha/presets/{id}/matches`.
/// Kept loose to match the gateway's JSON shape — see
/// `module-alpha-scanner/src/api/presets.rs::list_matches`.
#[derive(Debug, Clone, Deserialize)]
pub struct PresetMatchRow {
    pub call_id: String,
    pub chain_id: i16,
    pub token_address: String,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub price_usd: Option<String>,
    #[serde(default)]
    pub market_cap_usd: Option<String>,
    #[serde(default)]
    pub liquidity_usd: Option<String>,
    pub called_at: String,
    pub matched_at: String,
    pub preset_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_req() -> CreateIntentReq {
        CreateIntentReq {
            side: "buy".into(),
            input_mint: "So11111111111111111111111111111111111111112".into(),
            output_mint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".into(),
            amount_in_lamports: 100_000_000,
            slippage_bps: Some(100),
            submit_mode: Some("falcon_jito".into()),
            tip_lamports: Some(1_000_000),
            preset_id: None,
            bot_session_id: None,
            quote_snapshot: None,
            copy_config_id: None,
            client_token: None,
        }
    }

    #[test]
    fn create_intent_req_serialises_without_optional_fields() {
        // None fields must NOT appear on the wire — gateway treats
        // them as absent. Anything else would either fail the gateway's
        // strict deserializer or leak placeholder values.
        let json = serde_json::to_string(&base_req()).unwrap();
        assert!(!json.contains("preset_id"));
        assert!(!json.contains("bot_session_id"));
        assert!(!json.contains("quote_snapshot"));
        assert!(!json.contains("client_token"));
        // Required fields must be present.
        assert!(json.contains("\"side\":\"buy\""));
        assert!(json.contains("\"amount_in_lamports\":100000000"));
    }

    #[test]
    fn create_intent_req_serialises_client_token_when_set() {
        let mut r = base_req();
        r.client_token = Some("sig:abc-123:buy".into());
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"client_token\":\"sig:abc-123:buy\""));
    }

    #[test]
    fn create_intent_req_serialises_bot_session_id_when_set() {
        let mut r = base_req();
        r.bot_session_id = Some("11111111-2222-3333-4444-555555555555".into());
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"bot_session_id\":\"11111111-2222-3333-4444-555555555555\""));
    }

    #[test]
    fn create_intent_req_serialises_preset_id_when_set() {
        let mut r = base_req();
        r.preset_id = Some("aabbccdd-eeff-0011-2233-445566778899".into());
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"preset_id\":\"aabbccdd-eeff-0011-2233-445566778899\""));
    }

    #[test]
    fn create_intent_req_round_trips_via_gateway_shape() {
        // The gateway's domain::CreateIntentReq deserialises this — we
        // ensure the wire shape is at minimum self-compatible (same
        // crate's serialiser + reqwest::json -> the gateway parser).
        let mut r = base_req();
        r.preset_id = Some("p1".into());
        r.bot_session_id = Some("s1".into());
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["preset_id"], "p1");
        assert_eq!(json["bot_session_id"], "s1");
        assert_eq!(json["submit_mode"], "falcon_jito");
    }
}
