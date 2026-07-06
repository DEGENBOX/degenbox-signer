//! Background-refreshed cache for `getLatestBlockhash`.
//!
//! Stamping a Solana transaction with a recent blockhash is mandatory
//! and costs ~30–100 ms of RPC round-trip per trade. For memecoin
//! sniping this is wasted time on the hot path — we know we'll need a
//! blockhash on every trade, and blockhashes stay valid for ~60 s, so
//! we keep one cached and refresh it in the background.
//!
//! Design:
//! - background tokio task polls `getLatestBlockhash` every 400 ms
//!   (one Solana block-time) using commitment=`processed` for the
//!   freshest value the cluster can give us
//! - readers grab the cached hash via `get()` — fast path is a single
//!   `RwLock::read()` on a 32-byte `Hash` + an `Instant` compare
//! - if the cache is empty or stale (>2 s old), `get()` falls back to
//!   a direct RPC call and repopulates the cache on success
//!
//! Failures in the background loop are logged but never panic — the
//! cache simply ages out and the next `get()` does a direct fetch.

use crate::rpc::{RpcClient, RpcError};
use solana_sdk::hash::Hash;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Maximum age of a cached blockhash before `get()` falls back to a
/// direct fetch. Solana blockhashes are valid for ~150 slots (~60 s),
/// but for trading we want recency to maximise priority-fee
/// effectiveness during congestion. 2 s ≈ 5 blocks of headroom.
const MAX_AGE: Duration = Duration::from_secs(2);

/// How often the background refresher polls the RPC.
const REFRESH_INTERVAL: Duration = Duration::from_millis(400);

/// Inner cache cell. Tuple of (hash, when-fetched).
type CacheCell = Option<(Hash, Instant)>;

/// Cheaply-cloneable handle to the shared blockhash cache.
///
/// All clones share the same inner cell and the same background
/// refresher; constructing via `new()` spawns the refresher exactly
/// once per `RpcClient`.
#[derive(Clone)]
pub struct BlockhashCache {
    inner: Arc<RwLock<CacheCell>>,
    rpc: RpcClient,
}

impl BlockhashCache {
    /// Build a cache and spawn the background refresher.
    ///
    /// The spawn handle is dropped (detached) on purpose — the cache
    /// is expected to live for the process lifetime and the loop
    /// self-recovers on RPC errors.
    pub fn new(rpc: RpcClient) -> Self {
        let inner: Arc<RwLock<CacheCell>> = Arc::new(RwLock::new(None));
        let me = Self {
            inner: Arc::clone(&inner),
            rpc: rpc.clone(),
        };
        let refresher = me.clone();
        tokio::spawn(async move {
            refresher.run_refresher().await;
        });
        me
    }

    /// Build a cache **without** spawning the background refresher.
    /// Useful for unit tests and for environments (e.g. WASM) where
    /// tokio's spawner isn't available. Callers must populate the
    /// cache via direct `get()` calls or via `store_for_test()`.
    pub fn new_inert(rpc: RpcClient) -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
            rpc,
        }
    }

    /// Return a recent blockhash. Cache-first, with fall-back to a
    /// direct `getLatestBlockhash` call when the cache is empty or
    /// stale.
    pub async fn get(&self) -> Result<Hash, RpcError> {
        if let Some(hash) = self.read_fresh() {
            return Ok(hash);
        }
        let hash = self.rpc.get_latest_blockhash().await?;
        self.store(hash);
        Ok(hash)
    }

    /// Internal: read the cached value if it is younger than `MAX_AGE`.
    fn read_fresh(&self) -> Option<Hash> {
        let guard = self.inner.read().ok()?;
        guard
            .as_ref()
            .and_then(|(h, ts)| (ts.elapsed() < MAX_AGE).then_some(*h))
    }

    fn store(&self, hash: Hash) {
        if let Ok(mut guard) = self.inner.write() {
            *guard = Some((hash, Instant::now()));
        }
    }

    /// Test-only seed. Allows unit tests to inject a known hash
    /// without spawning a refresher.
    #[doc(hidden)]
    pub fn store_for_test(&self, hash: Hash) {
        self.store(hash);
    }

    async fn run_refresher(self) {
        let mut tick = tokio::time::interval(REFRESH_INTERVAL);
        // If we get behind (e.g. RPC stall), skip catch-up ticks
        // instead of bursting.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match self.rpc.get_latest_blockhash().await {
                Ok(h) => self.store(h),
                Err(e) => {
                    tracing::warn!(error = %e, "blockhash refresh failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::hash::Hash;

    #[test]
    fn fresh_value_round_trips() {
        let cache = BlockhashCache::new_inert(RpcClient::new("http://stub"));
        let h = Hash::new_unique();
        cache.store_for_test(h);
        assert_eq!(cache.read_fresh(), Some(h));
    }

    #[test]
    fn stale_value_returns_none() {
        let cache = BlockhashCache::new_inert(RpcClient::new("http://stub"));
        let h = Hash::new_unique();
        // Backdate the stored instant past MAX_AGE.
        {
            let mut guard = cache.inner.write().unwrap();
            *guard = Some((h, Instant::now() - MAX_AGE - Duration::from_millis(50)));
        }
        assert!(cache.read_fresh().is_none());
    }

    #[test]
    fn empty_cache_returns_none() {
        let cache = BlockhashCache::new_inert(RpcClient::new("http://stub"));
        assert!(cache.read_fresh().is_none());
    }
}
