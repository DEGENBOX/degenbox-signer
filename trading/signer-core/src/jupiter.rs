//! Thin Jupiter Swap API quote + swap client.
//!
//! Two-step flow:
//!   1. `quote(input_mint, output_mint, amount, slippage_bps)` returns
//!      a route + expected-out + price-impact.
//!   2. `swap(quote, user_pubkey, options)` returns the base64-encoded
//!      unsigned `VersionedTransaction` ready to be signed by the
//!      caller's keystore.
//!
//! We call Jupiter's mainnet endpoint directly. The signer is the
//! source of truth for the user's pubkey + slippage — we never trust
//! the gateway to decide either.
//!
//! API migration (May 2026): `quote-api.jup.ag` was sunset and no
//! longer resolves. Jupiter consolidated everything under
//! `api.jup.ag/swap/v1` — same request/response shapes, new host.
//! Quote = GET  /swap/v1/quote
//! Swap  = POST /swap/v1/swap

use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_BASE: &str = "https://api.jup.ag/swap/v1";

#[derive(Debug, Error)]
pub enum JupiterError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("jupiter responded with status {0}: {1}")]
    Status(u16, String),
    #[error("decode: {0}")]
    Decode(String),
}

#[derive(Clone)]
pub struct JupiterClient {
    http: reqwest::Client,
    base: String,
}

impl JupiterClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client builder"),
            base: DEFAULT_BASE.to_string(),
        }
    }

    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// `GET /quote?inputMint=…&outputMint=…&amount=…&slippageBps=…`.
    /// `amount` is in input-mint base units (lamports for SOL).
    pub async fn quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u16,
    ) -> Result<QuoteResponse, JupiterError> {
        let url = format!("{}/quote", self.base);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("inputMint", input_mint),
                ("outputMint", output_mint),
                ("amount", &amount.to_string()),
                ("slippageBps", &slippage_bps.to_string()),
                ("onlyDirectRoutes", "false"),
                ("restrictIntermediateTokens", "true"),
            ])
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(JupiterError::Status(status.as_u16(), body));
        }
        let q: QuoteResponse = resp
            .json()
            .await
            .map_err(|e| JupiterError::Decode(e.to_string()))?;
        Ok(q)
    }

    /// `POST /swap` — accepts the full quote response back as
    /// `quoteResponse`, plus user-pubkey + a few options. Returns the
    /// base64-encoded unsigned `VersionedTransaction`.
    pub async fn swap(
        &self,
        quote: &QuoteResponse,
        user_pubkey_b58: &str,
        opts: SwapOptions,
    ) -> Result<SwapResponse, JupiterError> {
        let url = format!("{}/swap", self.base);
        let body = SwapRequest {
            quote_response: quote.clone(),
            user_public_key: user_pubkey_b58.to_string(),
            wrap_and_unwrap_sol: opts.wrap_unwrap_sol,
            // Jupiter's "auto" priority-fee mode — uses the network's
            // recent-prioritization-fees percentile. The caller can
            // override with explicit lamports if they have a tighter
            // sniper SLA in mind.
            prioritization_fee_lamports: opts
                .priority_fee_lamports
                .map(|n| serde_json::Value::Number(n.into()))
                .unwrap_or_else(|| serde_json::Value::String("auto".into())),
            dynamic_compute_unit_limit: true,
            // Don't include a fee account — we're not collecting
            // platform fees from the swap-side here.
            fee_account: None,
        };

        let resp = self.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(JupiterError::Status(status.as_u16(), body));
        }
        resp.json::<SwapResponse>()
            .await
            .map_err(|e| JupiterError::Decode(e.to_string()))
    }
}

impl Default for JupiterClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Default)]
pub struct SwapOptions {
    /// Wraps SOL → wSOL on input + unwraps on output. Almost always
    /// what the user wants when buying tokens with SOL.
    pub wrap_unwrap_sol: bool,
    /// Explicit priority fee. None → use Jupiter's "auto" mode.
    pub priority_fee_lamports: Option<u64>,
}

// ─── wire types ────────────────────────────────────────────────────

/// `QuoteResponse` is a passthrough — we don't deconstruct routes,
/// we just hand the full opaque blob back to `/swap`. Defined as a
/// struct of structs so callers can read the fields they care about
/// (expected-out, price-impact) without parsing JSON manually.
///
/// Jupiter adds new fields over time; `#[serde(other)]` on the
/// passthrough means we don't break on additions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteResponse {
    #[serde(rename = "inputMint")]
    pub input_mint: String,
    #[serde(rename = "outputMint")]
    pub output_mint: String,
    #[serde(rename = "inAmount")]
    pub in_amount: String,
    #[serde(rename = "outAmount")]
    pub out_amount: String,
    #[serde(rename = "otherAmountThreshold")]
    pub other_amount_threshold: String,
    #[serde(rename = "swapMode")]
    pub swap_mode: String,
    #[serde(rename = "slippageBps")]
    pub slippage_bps: u16,
    #[serde(rename = "priceImpactPct")]
    pub price_impact_pct: String,
    /// All other fields Jupiter returns — preserved verbatim so the
    /// `/swap` POST round-trip works regardless of Jupiter version.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct SwapRequest {
    #[serde(rename = "quoteResponse")]
    quote_response: QuoteResponse,
    #[serde(rename = "userPublicKey")]
    user_public_key: String,
    #[serde(rename = "wrapAndUnwrapSol")]
    wrap_and_unwrap_sol: bool,
    #[serde(rename = "prioritizationFeeLamports")]
    prioritization_fee_lamports: serde_json::Value,
    #[serde(rename = "dynamicComputeUnitLimit")]
    dynamic_compute_unit_limit: bool,
    #[serde(rename = "feeAccount", skip_serializing_if = "Option::is_none")]
    fee_account: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SwapResponse {
    /// Base64-encoded `VersionedTransaction` ready to sign.
    #[serde(rename = "swapTransaction")]
    pub swap_transaction: String,
    #[serde(default, rename = "lastValidBlockHeight")]
    pub last_valid_block_height: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_response_round_trips() {
        // Jupiter's response shape is stable; this fixture pins the
        // fields we read directly + the passthrough behaviour for
        // unknown fields.
        let raw = serde_json::json!({
            "inputMint": "So11111111111111111111111111111111111111112",
            "outputMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "inAmount": "100000000",
            "outAmount": "21500000",
            "otherAmountThreshold": "21392500",
            "swapMode": "ExactIn",
            "slippageBps": 50,
            "priceImpactPct": "0.0123",
            "routePlan": [{"swapInfo": {"ammKey": "..."}}],
            "contextSlot": 268_000_000u64,
            "timeTaken": 0.012
        });
        let q: QuoteResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(q.input_mint, "So11111111111111111111111111111111111111112");
        assert_eq!(q.in_amount, "100000000");
        assert_eq!(q.out_amount, "21500000");
        assert_eq!(q.slippage_bps, 50);
        // The unknown fields preserved.
        assert!(q.extra.contains_key("routePlan"));
        assert!(q.extra.contains_key("contextSlot"));

        // Re-serialize: round-trip must include the extras.
        let back = serde_json::to_value(&q).unwrap();
        assert!(back.get("routePlan").is_some());
    }
}
