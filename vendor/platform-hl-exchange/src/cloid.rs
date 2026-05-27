//! Client-order-id (cloid) helpers.
//!
//! HL accepts a 16-byte hex `c` field on `OrderWire` for deduplication +
//! correlation. We mint it as `"0x" + first 16 bytes of UUID v7`. UUID
//! v7 gives us:
//!   - 128 bits of unique randomness (plus a millisecond timestamp
//!     prefix), so collisions are astronomically improbable;
//!   - sortable-by-time → DB queries that filter by recency stay fast;
//!   - matches the deterministic-cloid pattern used by the TypeScript
//!     reference (`generateCloid(sourceId, market, action)` → first 16
//!     bytes of SHA-256) in the byte-shape it lands on the wire.

pub fn new_cloid() -> String {
    let uuid = uuid::Uuid::now_v7();
    let bytes = uuid.as_bytes();
    let mut s = String::with_capacity(34);
    s.push_str("0x");
    s.push_str(&hex::encode(bytes));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloid_is_well_formed() {
        let c = new_cloid();
        // "0x" + 32 hex chars = 16 bytes.
        assert!(c.starts_with("0x"));
        assert_eq!(c.len(), 34);
        assert!(c[2..].chars().all(|x| x.is_ascii_hexdigit()));
    }

    #[test]
    fn successive_cloids_differ() {
        // 1 000 mints with no collision — sanity check on randomness.
        let mut set = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(set.insert(new_cloid()));
        }
    }
}
