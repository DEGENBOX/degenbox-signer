//! Minimal Hyperliquid `/info` client used by the close / TP-SL
//! payload handlers.
//!
//! Why a local copy instead of re-using `module_hyperliquid`'s
//! `InfoClient`? — The signer-desktop is intentionally NOT part of the
//! backend workspace (users build it as a standalone artifact). The
//! backend `InfoClient` drags in DB types, NATS, sqlx and a lot more.
//! Here we only ever call `clearinghouseState`, so a 50-line client is
//! the right size.
//!
//! Behind a trait so the payload handlers in `signing.rs` can be
//! unit-tested without touching the network.

use async_trait::async_trait;
use platform_hl_exchange::Network;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InfoError {
    #[error("hl info http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("hl info status {0}: {1}")]
    Status(u16, String),
    #[error("hl info decode: {0}")]
    Decode(String),
    #[error("no open position for {0}")]
    NoPosition(String),
}

/// One row from `clearinghouseState.assetPositions[…]` — only the
/// fields the signer cares about.
#[derive(Debug, Clone)]
pub struct LivePosition {
    /// Echo of the matched coin string. Mostly diagnostic — callers
    /// already know which coin they asked for; kept on the struct so
    /// log lines and error messages can prove which row was used
    /// when HL ships slightly-different casings.
    #[allow(dead_code)]
    pub coin: String,
    /// Signed position size. Positive = long, negative = short, zero
    /// is filtered out at parse time.
    pub szi: Decimal,
}

/// Trait so tests can swap in a deterministic fake without hitting HL.
#[async_trait]
pub trait InfoClient: Send + Sync {
    /// Look up a single open position by coin (case-insensitive).
    /// Returns `NoPosition` when the wallet has no live size in
    /// `coin` — the caller's instruction is effectively a no-op and
    /// is reported back to the server as `cancelled` (no order to
    /// submit) rather than `failed` (HL refused).
    async fn position_for(&self, account: &str, coin: &str) -> Result<LivePosition, InfoError>;
}

/// Real HTTP client. Talks to `https://api.hyperliquid.xyz/info` (or
/// the testnet equivalent) — no auth, just a JSON POST.
#[derive(Clone)]
pub struct HttpInfoClient {
    http: reqwest::Client,
    base: String,
}

impl HttpInfoClient {
    pub fn new(network: Network) -> Result<Self, InfoError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            http,
            base: info_url(network).to_string(),
        })
    }
}

fn info_url(network: Network) -> &'static str {
    if network.is_mainnet() {
        "https://api.hyperliquid.xyz/info"
    } else {
        "https://api.hyperliquid-testnet.xyz/info"
    }
}

#[derive(Debug, Deserialize)]
struct ClearinghouseState {
    #[serde(rename = "assetPositions", default)]
    asset_positions: Vec<AssetPositionWrap>,
}

#[derive(Debug, Deserialize)]
struct AssetPositionWrap {
    position: AssetPosition,
}

#[derive(Debug, Deserialize)]
struct AssetPosition {
    coin: String,
    /// HL ships every numeric as a JSON string.
    szi: String,
}

#[async_trait]
impl InfoClient for HttpInfoClient {
    async fn position_for(&self, account: &str, coin: &str) -> Result<LivePosition, InfoError> {
        let body = serde_json::json!({"type": "clearinghouseState", "user": account});
        let resp = self.http.post(&self.base).json(&body).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).to_string();
            return Err(InfoError::Status(status.as_u16(), truncate(&body, 512)));
        }
        let state: ClearinghouseState =
            serde_json::from_slice(&bytes).map_err(|e| InfoError::Decode(format!("{e}")))?;
        find_position(&state, coin)
    }
}

fn find_position(state: &ClearinghouseState, coin: &str) -> Result<LivePosition, InfoError> {
    let coin_upper = coin.to_ascii_uppercase();
    for wrap in &state.asset_positions {
        if wrap.position.coin.to_ascii_uppercase() != coin_upper {
            continue;
        }
        let szi = Decimal::from_str(&wrap.position.szi)
            .map_err(|e| InfoError::Decode(format!("szi parse: {e}")))?;
        if szi.is_zero() {
            // Stale-stub semantics: treat zero-size as no position.
            return Err(InfoError::NoPosition(coin_upper));
        }
        return Ok(LivePosition {
            coin: wrap.position.coin.clone(),
            szi,
        });
    }
    Err(InfoError::NoPosition(coin_upper))
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…(+{} more bytes)", &s[..n], s.len() - n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn state_from(json: serde_json::Value) -> ClearinghouseState {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn info_url_picks_testnet_or_mainnet() {
        assert_eq!(
            info_url(Network::Mainnet),
            "https://api.hyperliquid.xyz/info"
        );
        assert_eq!(
            info_url(Network::Testnet),
            "https://api.hyperliquid-testnet.xyz/info"
        );
    }

    #[test]
    fn find_position_matches_case_insensitive() {
        let s = state_from(serde_json::json!({
            "assetPositions": [
                {"position": {"coin": "BTC", "szi": "0.5"}},
                {"position": {"coin": "ETH", "szi": "-2.0"}},
            ]
        }));
        let p = find_position(&s, "btc").unwrap();
        assert_eq!(p.coin, "BTC");
        assert_eq!(p.szi, dec!(0.5));

        let p2 = find_position(&s, "ETH").unwrap();
        assert_eq!(p2.szi, dec!(-2.0));
    }

    #[test]
    fn find_position_returns_no_position_when_missing() {
        let s = state_from(serde_json::json!({"assetPositions": []}));
        let err = find_position(&s, "BTC").unwrap_err();
        assert!(matches!(err, InfoError::NoPosition(_)));
    }

    #[test]
    fn find_position_treats_zero_size_as_no_position() {
        let s = state_from(serde_json::json!({
            "assetPositions": [
                {"position": {"coin": "BTC", "szi": "0"}},
            ]
        }));
        let err = find_position(&s, "BTC").unwrap_err();
        assert!(matches!(err, InfoError::NoPosition(_)));
    }

    #[test]
    fn find_position_rejects_garbage_size() {
        let s = state_from(serde_json::json!({
            "assetPositions": [
                {"position": {"coin": "BTC", "szi": "not-a-number"}},
            ]
        }));
        let err = find_position(&s, "BTC").unwrap_err();
        assert!(matches!(err, InfoError::Decode(_)));
    }
}
