//! Minimal Hyperliquid `/info` client.
//!
//! Canonical merge of `hl-signer-desktop/src/hl_info.rs` (prod CLI) and
//! the signer-app port. Used by the close / TP-SL payload handlers (live
//! position lookup, closedPnl), by balance surfaces (`account_summary`
//! off the MASTER account), and by offline kill-switch flows
//! (`open_orders` / `all_positions`). Behind a trait so the signing
//! handlers can be unit-tested without the network.

use async_trait::async_trait;
use platform_hl_exchange::{parse_user_fills, sum_closed_pnl_for_oid, Network};
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

/// Account-level balance summary for dashboards. Sourced from the
/// MASTER account's `clearinghouseState`. The agent address reads $0, so
/// callers MUST pass the master.
#[derive(Debug, Clone, Default)]
pub struct AccountSummary {
    pub account_value_usd: Option<String>,
    pub withdrawable_usd: Option<String>,
    pub positions: Vec<LivePosition>,
}

/// One row from `clearinghouseState.assetPositions[…]` — only the
/// fields the signer family cares about.
#[derive(Debug, Clone)]
pub struct LivePosition {
    /// Echo of the matched coin string (diagnostic — proves which row
    /// was used when HL ships slightly-different casings).
    pub coin: String,
    /// Signed position size. Positive = long, negative = short, zero
    /// is filtered out at parse time.
    pub szi: Decimal,
    /// Unrealised PnL (USD) on the position, if HL reports it.
    pub unrealized_pnl: Option<String>,
    /// Average entry price, if HL reports it.
    pub entry_px: Option<String>,
}

/// One resting open order (kill-switch shape).
#[derive(Debug, Clone)]
pub struct OpenOrder {
    pub coin: String,
    pub oid: i64,
}

/// Trait so tests can swap in a deterministic fake without hitting HL.
#[async_trait]
pub trait InfoClient: Send + Sync {
    /// Look up a single open position by coin (case-insensitive).
    /// Returns `NoPosition` when the wallet has no live size in `coin` —
    /// the caller's instruction is effectively a no-op and is reported
    /// back to the server as `cancelled` rather than `failed`.
    async fn position_for(&self, account: &str, coin: &str) -> Result<LivePosition, InfoError>;

    /// Realised PnL for a just-submitted order, summed across all
    /// `userFills` rows matching `oid`. `Ok(None)` = not indexed (the
    /// caller MUST report `closed_pnl: None`, never a fabricated zero).
    async fn closed_pnl_for_oid(
        &self,
        _account: &str,
        _oid: u64,
    ) -> Result<Option<Decimal>, InfoError> {
        Ok(None)
    }

    /// Perp universe metadata (coin → asset_id + sz_decimals), used to
    /// round order sizes to HL's allowed precision. Default empty so test
    /// fakes need not implement it.
    async fn perp_meta(&self) -> Result<Vec<MetaAsset>, InfoError> {
        Ok(Vec::new())
    }

    /// Current mid (mark) price for a perp coin, from HL `allMids`. Used
    /// to build a VALID guard price for reduce-only market closes.
    async fn mid_price(&self, _coin: &str) -> Result<Option<Decimal>, InfoError> {
        Ok(None)
    }
}

/// Real HTTP client. Talks to `https://api.hyperliquid.xyz/info` (or the
/// testnet equivalent) — no auth, just a JSON POST.
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

    /// Raw `/info` POST → response bytes.
    async fn post(&self, body: &serde_json::Value) -> Result<Vec<u8>, InfoError> {
        let resp = self.http.post(&self.base).json(body).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            let txt = String::from_utf8_lossy(&bytes).to_string();
            return Err(InfoError::Status(status.as_u16(), truncate(&txt, 512)));
        }
        Ok(bytes.to_vec())
    }

    /// Perp universe — `coin → (asset_id, szDecimals)`. The asset id is
    /// the index into HL's `universe` array, required to build cancel +
    /// order actions offline (kill-switch use; no server to resolve it).
    pub async fn meta(&self) -> Result<Vec<MetaAsset>, InfoError> {
        let bytes = self.post(&serde_json::json!({"type": "meta"})).await?;
        let meta: MetaResp =
            serde_json::from_slice(&bytes).map_err(|e| InfoError::Decode(format!("meta: {e}")))?;
        Ok(meta
            .universe
            .into_iter()
            .enumerate()
            .map(|(i, a)| MetaAsset {
                name: a.name,
                asset_id: i as u32,
                sz_decimals: a.sz_decimals,
            })
            .collect())
    }

    /// All resting open orders for `account` (`coin` + `oid`). Used by
    /// kill-switch flows to cancel everything.
    pub async fn open_orders(&self, account: &str) -> Result<Vec<OpenOrder>, InfoError> {
        let bytes = self
            .post(&serde_json::json!({"type": "openOrders", "user": account}))
            .await?;
        let orders: Vec<OpenOrderRaw> = serde_json::from_slice(&bytes)
            .map_err(|e| InfoError::Decode(format!("openOrders: {e}")))?;
        Ok(orders
            .into_iter()
            .map(|o| OpenOrder {
                coin: o.coin,
                oid: o.oid,
            })
            .collect())
    }

    /// Every non-zero open position for `account`. Used by kill-switch
    /// flows to flatten the book.
    pub async fn all_positions(&self, account: &str) -> Result<Vec<LivePosition>, InfoError> {
        Ok(self.account_summary(account).await?.positions)
    }

    /// Account value + withdrawable + every non-zero open position in ONE
    /// `clearinghouseState` round-trip. `account` MUST be the MASTER
    /// wallet — the agent address always reads $0.
    pub async fn account_summary(&self, account: &str) -> Result<AccountSummary, InfoError> {
        let bytes = self
            .post(&serde_json::json!({"type": "clearinghouseState", "user": account}))
            .await?;
        let state: ClearinghouseState = serde_json::from_slice(&bytes)
            .map_err(|e| InfoError::Decode(format!("clearinghouseState: {e}")))?;
        let mut positions = Vec::new();
        for wrap in &state.asset_positions {
            let szi = Decimal::from_str(&wrap.position.szi)
                .map_err(|e| InfoError::Decode(format!("szi parse: {e}")))?;
            if szi.is_zero() {
                continue;
            }
            positions.push(LivePosition {
                coin: wrap.position.coin.clone(),
                szi,
                unrealized_pnl: wrap.position.unrealized_pnl.clone(),
                entry_px: wrap.position.entry_px.clone(),
            });
        }
        Ok(AccountSummary {
            account_value_usd: state.margin_summary.and_then(|m| m.account_value),
            withdrawable_usd: state.withdrawable,
            positions,
        })
    }
}

/// One perp asset from `meta.universe`. `asset_id` = array index — the
/// id HL's order/cancel actions key on.
#[derive(Debug, Clone)]
pub struct MetaAsset {
    pub name: String,
    pub asset_id: u32,
    /// HL's allowed size precision for this perp (decimals). Sizes must
    /// be truncated to this many decimals or HL rejects the order.
    pub sz_decimals: u32,
}

#[derive(Debug, Deserialize)]
struct MetaResp {
    #[serde(default)]
    universe: Vec<MetaUniverseAsset>,
}

#[derive(Debug, Deserialize)]
struct MetaUniverseAsset {
    name: String,
    #[serde(rename = "szDecimals", default)]
    sz_decimals: u32,
}

#[derive(Debug, Deserialize)]
struct OpenOrderRaw {
    coin: String,
    oid: i64,
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
    #[serde(rename = "marginSummary", default)]
    margin_summary: Option<MarginSummary>,
    /// Free / withdrawable USD. HL ships it as a JSON string.
    #[serde(default)]
    withdrawable: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct MarginSummary {
    #[serde(rename = "accountValue", default)]
    account_value: Option<String>,
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
    #[serde(rename = "unrealizedPnl", default)]
    unrealized_pnl: Option<String>,
    #[serde(rename = "entryPx", default)]
    entry_px: Option<String>,
}

#[async_trait]
impl InfoClient for HttpInfoClient {
    async fn position_for(&self, account: &str, coin: &str) -> Result<LivePosition, InfoError> {
        let bytes = self
            .post(&serde_json::json!({"type": "clearinghouseState", "user": account}))
            .await?;
        let state: ClearinghouseState =
            serde_json::from_slice(&bytes).map_err(|e| InfoError::Decode(format!("{e}")))?;
        find_position(&state, coin)
    }

    async fn perp_meta(&self) -> Result<Vec<MetaAsset>, InfoError> {
        HttpInfoClient::meta(self).await
    }

    async fn mid_price(&self, coin: &str) -> Result<Option<Decimal>, InfoError> {
        let bytes = self.post(&serde_json::json!({"type": "allMids"})).await?;
        // allMids shape: { "BTC": "60000.0", "ETH": "3000.0", ... }
        let mids: std::collections::HashMap<String, String> = serde_json::from_slice(&bytes)
            .map_err(|e| InfoError::Decode(format!("allMids: {e}")))?;
        let val = mids
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(coin))
            .and_then(|(_, v)| Decimal::from_str_exact(v).ok());
        Ok(val)
    }

    async fn closed_pnl_for_oid(
        &self,
        account: &str,
        oid: u64,
    ) -> Result<Option<Decimal>, InfoError> {
        // HL fills are indexed async; a market close usually shows up
        // within a few hundred ms but can lag the `/exchange` ack. Poll
        // a few times with a short backoff before giving up. On timeout
        // we return Ok(None) and the caller reports closed_pnl: None.
        const ATTEMPTS: u32 = 5;
        const DELAY: Duration = Duration::from_millis(400);
        let body = serde_json::json!({"type": "userFills", "user": account});
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(DELAY).await;
            }
            let bytes = self.post(&body).await?;
            let fills = parse_user_fills(&bytes)
                .map_err(|e| InfoError::Decode(format!("userFills: {e}")))?;
            if let Some(pnl) = sum_closed_pnl_for_oid(&fills, oid) {
                return Ok(Some(pnl));
            }
            // Not indexed yet — retry.
        }
        Ok(None)
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
            unrealized_pnl: wrap.position.unrealized_pnl.clone(),
            entry_px: wrap.position.entry_px.clone(),
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
        assert_eq!(find_position(&s, "ETH").unwrap().szi, dec!(-2.0));
    }

    #[test]
    fn find_position_returns_no_position_when_missing_or_zero() {
        let s = state_from(serde_json::json!({"assetPositions": []}));
        assert!(matches!(
            find_position(&s, "BTC"),
            Err(InfoError::NoPosition(_))
        ));
        let z = state_from(serde_json::json!({
            "assetPositions": [{"position": {"coin": "BTC", "szi": "0"}}]
        }));
        assert!(matches!(
            find_position(&z, "BTC"),
            Err(InfoError::NoPosition(_))
        ));
    }

    #[test]
    fn find_position_rejects_garbage_size() {
        let s = state_from(serde_json::json!({
            "assetPositions": [
                {"position": {"coin": "BTC", "szi": "not-a-number"}},
            ]
        }));
        assert!(matches!(
            find_position(&s, "BTC"),
            Err(InfoError::Decode(_))
        ));
    }

    #[test]
    fn position_carries_pnl_and_entry_when_present() {
        let s = state_from(serde_json::json!({
            "assetPositions": [
                {"position": {"coin": "BTC", "szi": "0.5",
                              "unrealizedPnl": "12.3", "entryPx": "60000"}},
            ]
        }));
        let p = find_position(&s, "BTC").unwrap();
        assert_eq!(p.unrealized_pnl.as_deref(), Some("12.3"));
        assert_eq!(p.entry_px.as_deref(), Some("60000"));
    }
}
