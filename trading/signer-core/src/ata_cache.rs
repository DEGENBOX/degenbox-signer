//! Cache of `(owner, mint)` pairs for which we already know the
//! Associated Token Account exists.
//!
//! Every DEX swap we build today pre-pends `CreateIdempotent` ATA
//! instructions to guarantee the source/destination accounts exist.
//! When the account already exists the instruction short-circuits to
//! a no-op — but it still costs ~3.5k CU **and** ~120 bytes of tx
//! payload. For a high-frequency sniper that's wasted budget on every
//! repeat trade for the same wallet+mint.
//!
//! This cache tracks pairs we've already seen succeed. The bot engine
//! consults it before building a swap tx and, when the pair is known,
//! omits the `CreateIdempotent` instruction.
//!
//! Correctness: a false positive (cache says known but ATA was closed
//! after we last saw it) will make the swap fail with a "missing
//! account" runtime error — a graceful, recoverable failure. We
//! invalidate the entry on any such failure so the next attempt
//! includes the create-IX again. A false negative is harmless: we
//! redundantly include the no-op IX.

use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

/// Shared, cheaply-cloneable handle to the ATA-existence cache.
///
/// Reads (`is_known`) take a shared `RwLock::read()` and are
/// negligible cost in practice (32-byte pubkey hashing + set lookup).
#[derive(Clone, Default)]
pub struct AtaCache {
    inner: Arc<RwLock<HashSet<(Pubkey, Pubkey)>>>,
}

impl AtaCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// True if we have seen `(owner, mint)`'s ATA exist before.
    pub fn is_known(&self, owner: &Pubkey, mint: &Pubkey) -> bool {
        self.inner
            .read()
            .map(|g| g.contains(&(*owner, *mint)))
            .unwrap_or(false)
    }

    /// Record that the ATA for `(owner, mint)` exists. Idempotent.
    pub fn mark_known(&self, owner: Pubkey, mint: Pubkey) {
        if let Ok(mut g) = self.inner.write() {
            g.insert((owner, mint));
        }
    }

    /// Remove `(owner, mint)` from the cache. Call this on any tx
    /// failure that suggests the ATA was missing — next build will
    /// include the create-IX again.
    pub fn invalidate(&self, owner: &Pubkey, mint: &Pubkey) {
        if let Ok(mut g) = self.inner.write() {
            g.remove(&(*owner, *mint));
        }
    }

    /// Number of cached pairs. Used in tests + diagnostics.
    pub fn len(&self) -> usize {
        self.inner.read().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    #[test]
    fn unknown_pair_returns_false() {
        let cache = AtaCache::new();
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        assert!(!cache.is_known(&owner, &mint));
    }

    #[test]
    fn mark_then_check_round_trips() {
        let cache = AtaCache::new();
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        cache.mark_known(owner, mint);
        assert!(cache.is_known(&owner, &mint));
    }

    #[test]
    fn mark_known_is_per_pair() {
        let cache = AtaCache::new();
        let owner = Pubkey::new_unique();
        let mint_a = Pubkey::new_unique();
        let mint_b = Pubkey::new_unique();
        cache.mark_known(owner, mint_a);
        assert!(cache.is_known(&owner, &mint_a));
        assert!(!cache.is_known(&owner, &mint_b));
    }

    #[test]
    fn invalidate_removes_entry() {
        let cache = AtaCache::new();
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        cache.mark_known(owner, mint);
        assert!(cache.is_known(&owner, &mint));
        cache.invalidate(&owner, &mint);
        assert!(!cache.is_known(&owner, &mint));
    }

    #[test]
    fn clone_shares_inner_state() {
        let cache_a = AtaCache::new();
        let cache_b = cache_a.clone();
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        cache_a.mark_known(owner, mint);
        // The clone sees the same entry — both handles point at the
        // same `Arc<RwLock<...>>`.
        assert!(cache_b.is_known(&owner, &mint));
    }

    #[test]
    fn mark_known_is_idempotent() {
        let cache = AtaCache::new();
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        cache.mark_known(owner, mint);
        cache.mark_known(owner, mint);
        assert_eq!(cache.len(), 1);
    }
}
