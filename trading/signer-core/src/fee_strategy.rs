//! Per-DEX compute-unit limits + global priority-fee tiers.
//!
//! Both numbers determine how much we end up paying for inclusion:
//!
//! - **CU limit** caps the worst-case compute we're willing to spend.
//!   Setting it too high wastes priority budget; too low and the tx
//!   reverts mid-instruction. Today's hardcoded values are conservative
//!   guesses; this module lets us tune them per-DEX and ship the
//!   tuned values without re-deploying.
//!
//! - **CU price** (micro-lamports per CU) is the per-unit priority
//!   fee. On a non-contested slot, ~50 µLam/CU lands fine; under
//!   contention you can pay 10-100x more. The fee poller (when wired
//!   up against a `getPriorityFeeEstimate`-capable RPC like Helius)
//!   keeps the tier values fresh.
//!
//! Tiers (`FeeTier`) select how aggressive we are: `Fast` is the
//!  baseline, `Turbo` ~3x, `Max` ~10x. The bot/UI picks a tier per
//! trade based on user intent (auto-buys = Turbo, snipe = Max).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Which on-chain venue the tx is targeting. Used as the lookup key
/// for CU limits.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum DexId {
    PumpFun,
    PumpFunAmm,
    RaydiumAmmV4,
    Jupiter,
}

/// Trade direction. CU consumption tends to differ between buys (often
/// include ATA-creates) and sells (no ATA on the input side).
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

/// How aggressive we are about landing. The poller writes one
/// fee-per-CU value per tier (typically p50 / p75 / p95 of the
/// network's current priority-fee distribution).
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeeTier {
    Fast,
    Turbo,
    Max,
}

/// Resolved parameters ready to drop into the compute-budget IXs.
#[derive(Debug, Clone, Copy)]
pub struct FeeParams {
    pub cu_limit: u32,
    pub cu_price_micro_lamports: u64,
}

/// Conservative defaults baked in so a freshly-booted daemon can
/// trade before any background poller has run.
const DEFAULT_PRICE_FAST: u64 = 50_000;
const DEFAULT_PRICE_TURBO: u64 = 150_000;
const DEFAULT_PRICE_MAX: u64 = 500_000;

fn default_cu_limit(dex: DexId, side: Side) -> u32 {
    match (dex, side) {
        // Bonding-curve buy includes ATA-create + buy IX, ~60-80k
        // observed; 120k gives a 50% buffer for slot-load variance.
        (DexId::PumpFun, Side::Buy) => 120_000,
        // Bonding-curve sell is leaner (no ATA-create).
        (DexId::PumpFun, Side::Sell) => 100_000,
        // PumpSwap (graduated) hits more accounts than classic.
        (DexId::PumpFunAmm, Side::Buy) => 200_000,
        (DexId::PumpFunAmm, Side::Sell) => 150_000,
        // Raydium v4 invokes the OpenBook CPI — highest CU of the bunch.
        (DexId::RaydiumAmmV4, Side::Buy) => 220_000,
        (DexId::RaydiumAmmV4, Side::Sell) => 180_000,
        // Jupiter sets its own CU budget internally; the field is
        // informational only for this venue. Keep a generous cap.
        (DexId::Jupiter, _) => 400_000,
    }
}

struct Inner {
    cu_limits: HashMap<(DexId, Side), u32>,
    prices: HashMap<FeeTier, u64>,
}

impl Default for Inner {
    fn default() -> Self {
        let mut prices = HashMap::new();
        prices.insert(FeeTier::Fast, DEFAULT_PRICE_FAST);
        prices.insert(FeeTier::Turbo, DEFAULT_PRICE_TURBO);
        prices.insert(FeeTier::Max, DEFAULT_PRICE_MAX);
        Self {
            cu_limits: HashMap::new(),
            prices,
        }
    }
}

/// Cheaply-cloneable handle to the shared fee strategy.
#[derive(Clone, Default)]
pub struct FeeStrategy {
    inner: Arc<RwLock<Inner>>,
}

impl FeeStrategy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve the CU limit + CU price for a trade.
    pub fn get(&self, dex: DexId, side: Side, tier: FeeTier) -> FeeParams {
        let guard = self.inner.read();
        let cu_limit = guard
            .as_ref()
            .ok()
            .and_then(|g| g.cu_limits.get(&(dex, side)).copied())
            .unwrap_or_else(|| default_cu_limit(dex, side));
        let cu_price_micro_lamports = guard
            .as_ref()
            .ok()
            .and_then(|g| g.prices.get(&tier).copied())
            .unwrap_or(match tier {
                FeeTier::Fast => DEFAULT_PRICE_FAST,
                FeeTier::Turbo => DEFAULT_PRICE_TURBO,
                FeeTier::Max => DEFAULT_PRICE_MAX,
            });
        FeeParams {
            cu_limit,
            cu_price_micro_lamports,
        }
    }

    /// Override the CU limit for a specific (DEX, side). Used by
    /// operators who want to lock in a tuned value, and by the future
    /// observed-CU recorder.
    pub fn set_cu_limit(&self, dex: DexId, side: Side, cu_limit: u32) {
        if let Ok(mut g) = self.inner.write() {
            g.cu_limits.insert((dex, side), cu_limit);
        }
    }

    /// Record an observed CU consumption from a confirmed tx and
    /// update the stored limit to `max(observed * 1.2, current)`.
    /// Keeps the limit roomy enough for the next trade without ever
    /// shrinking below historical evidence.
    ///
    /// Floored at 25_000 (Solana's minimum useful budget) and capped
    /// at 1_400_000 (the per-tx max).
    pub fn record_observed_cu(&self, dex: DexId, side: Side, observed: u32) {
        let with_buffer = ((observed as u64) * 12 / 10).clamp(25_000, 1_400_000) as u32;
        if let Ok(mut g) = self.inner.write() {
            let key = (dex, side);
            let current = g
                .cu_limits
                .get(&key)
                .copied()
                .unwrap_or_else(|| default_cu_limit(dex, side));
            // Take the larger of current and the observed-buffered
            // value so a single low-CU outlier doesn't cause the next
            // trade to revert.
            let next = current.max(with_buffer);
            g.cu_limits.insert(key, next);
        }
    }

    /// Override a priority-fee tier directly. The background poller
    /// uses this when it has fresh `getPriorityFeeEstimate` data.
    pub fn set_priority_fee(&self, tier: FeeTier, micro_lamports_per_cu: u64) {
        if let Ok(mut g) = self.inner.write() {
            g.prices.insert(tier, micro_lamports_per_cu);
        }
    }

    /// Snapshot of all tier prices. Diagnostic + tests.
    pub fn snapshot_prices(&self) -> HashMap<FeeTier, u64> {
        self.inner
            .read()
            .map(|g| g.prices.clone())
            .unwrap_or_default()
    }
}

// ─── priority-fee poller ────────────────────────────────────────────

/// Helius `getPriorityFeeEstimate` poller. Spawns a background task
/// that refreshes the three fee tiers every `interval` (default 2 s)
/// from the Helius RPC endpoint.
///
/// Returns immediately; the spawn handle is detached because the
/// strategy is expected to live for the process lifetime.
///
/// Skips silently when `helius_rpc_url` is empty — the daemon can
/// always call this and let the strategy fall back to the static
/// defaults when no Helius key is configured.
pub fn spawn_priority_fee_poller(strategy: FeeStrategy, helius_rpc_url: String) {
    if helius_rpc_url.is_empty() {
        tracing::info!("priority-fee poller: HELIUS_RPC_URL unset, using static defaults");
        return;
    }
    tokio::spawn(async move {
        let http = match reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error=%e, "priority-fee poller: client build failed, exiting");
                return;
            }
        };
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match fetch_priority_fee(&http, &helius_rpc_url).await {
                Ok((fast, turbo, max)) => {
                    strategy.set_priority_fee(FeeTier::Fast, fast);
                    strategy.set_priority_fee(FeeTier::Turbo, turbo);
                    strategy.set_priority_fee(FeeTier::Max, max);
                }
                Err(e) => {
                    tracing::debug!(error=%e, "priority-fee poller: fetch failed");
                }
            }
        }
    });
}

async fn fetch_priority_fee(
    http: &reqwest::Client,
    url: &str,
) -> Result<(u64, u64, u64), anyhow::Error> {
    // Helius API supports `getPriorityFeeEstimate` with
    // `options.includeAllPriorityFeeLevels = true`. We request that
    // and pick three points off the resulting distribution.
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getPriorityFeeEstimate",
        "params": [{
            "options": {
                "includeAllPriorityFeeLevels": true
            }
        }]
    });
    let resp = http.post(url).json(&req).send().await?;
    let body: serde_json::Value = resp.json().await?;
    parse_helius_levels(&body)
}

fn parse_helius_levels(body: &serde_json::Value) -> Result<(u64, u64, u64), anyhow::Error> {
    let levels = body
        .get("result")
        .and_then(|r| r.get("priorityFeeLevels"))
        .ok_or_else(|| anyhow::anyhow!("missing result.priorityFeeLevels"))?;
    // Helius returns fields like `medium`, `high`, `veryHigh`, `unsafeMax`.
    // Map: medium → Fast (Tier baseline), high → Turbo, veryHigh → Max.
    let pick = |key: &str| -> Option<u64> {
        levels
            .get(key)
            .and_then(|v| v.as_f64())
            .map(|f| f.max(0.0) as u64)
    };
    let fast = pick("medium").unwrap_or(DEFAULT_PRICE_FAST);
    let turbo = pick("high").unwrap_or(DEFAULT_PRICE_TURBO);
    let max = pick("veryHigh").unwrap_or(DEFAULT_PRICE_MAX);
    Ok((fast, turbo, max))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_return_baked_in_values() {
        let s = FeeStrategy::new();
        let p = s.get(DexId::PumpFun, Side::Buy, FeeTier::Fast);
        assert_eq!(p.cu_limit, 120_000);
        assert_eq!(p.cu_price_micro_lamports, DEFAULT_PRICE_FAST);
    }

    #[test]
    fn tiers_distinct_prices() {
        let s = FeeStrategy::new();
        let fast = s
            .get(DexId::PumpFun, Side::Buy, FeeTier::Fast)
            .cu_price_micro_lamports;
        let turbo = s
            .get(DexId::PumpFun, Side::Buy, FeeTier::Turbo)
            .cu_price_micro_lamports;
        let max = s
            .get(DexId::PumpFun, Side::Buy, FeeTier::Max)
            .cu_price_micro_lamports;
        assert!(fast < turbo && turbo < max);
    }

    #[test]
    fn set_cu_limit_overrides_default() {
        let s = FeeStrategy::new();
        s.set_cu_limit(DexId::RaydiumAmmV4, Side::Buy, 300_000);
        assert_eq!(
            s.get(DexId::RaydiumAmmV4, Side::Buy, FeeTier::Fast)
                .cu_limit,
            300_000
        );
        // Other (dex, side) unaffected.
        assert_eq!(
            s.get(DexId::RaydiumAmmV4, Side::Sell, FeeTier::Fast)
                .cu_limit,
            default_cu_limit(DexId::RaydiumAmmV4, Side::Sell)
        );
    }

    #[test]
    fn record_observed_adds_buffer_and_keeps_max() {
        let s = FeeStrategy::new();
        // Observed 100k → +20% buffer = 120k = current PumpFun buy default.
        s.record_observed_cu(DexId::PumpFun, Side::Buy, 100_000);
        let p = s.get(DexId::PumpFun, Side::Buy, FeeTier::Fast);
        assert!(p.cu_limit >= 120_000);
        // A lower observed value MUST NOT shrink the limit.
        s.record_observed_cu(DexId::PumpFun, Side::Buy, 60_000);
        let p2 = s.get(DexId::PumpFun, Side::Buy, FeeTier::Fast);
        assert_eq!(p2.cu_limit, p.cu_limit);
    }

    #[test]
    fn record_observed_caps_at_tx_max() {
        let s = FeeStrategy::new();
        s.record_observed_cu(DexId::PumpFun, Side::Buy, 10_000_000);
        assert_eq!(
            s.get(DexId::PumpFun, Side::Buy, FeeTier::Fast).cu_limit,
            1_400_000
        );
    }

    #[test]
    fn set_priority_fee_overrides_default() {
        let s = FeeStrategy::new();
        s.set_priority_fee(FeeTier::Turbo, 999_999);
        assert_eq!(
            s.get(DexId::PumpFun, Side::Buy, FeeTier::Turbo)
                .cu_price_micro_lamports,
            999_999
        );
    }

    #[test]
    fn parse_helius_levels_happy_path() {
        let body = serde_json::json!({
            "result": {
                "priorityFeeLevels": {
                    "min": 0,
                    "low": 100,
                    "medium": 1000,
                    "high": 5000,
                    "veryHigh": 25000,
                    "unsafeMax": 1000000
                }
            }
        });
        let (fast, turbo, max) = parse_helius_levels(&body).unwrap();
        assert_eq!((fast, turbo, max), (1000, 5000, 25000));
    }

    #[test]
    fn parse_helius_levels_missing_fields_fall_back_to_defaults() {
        let body = serde_json::json!({ "result": { "priorityFeeLevels": {} } });
        let (fast, turbo, max) = parse_helius_levels(&body).unwrap();
        assert_eq!(
            (fast, turbo, max),
            (DEFAULT_PRICE_FAST, DEFAULT_PRICE_TURBO, DEFAULT_PRICE_MAX)
        );
    }

    #[test]
    fn parse_helius_levels_missing_result_errors() {
        let body = serde_json::json!({ "error": "lol" });
        assert!(parse_helius_levels(&body).is_err());
    }
}
