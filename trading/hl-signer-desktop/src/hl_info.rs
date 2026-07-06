//! Hyperliquid `/info` client — re-pointed onto the canonical copy in
//! `signer-core` (`hl::info`, a superset merge of this file: same
//! `clearinghouseState` / `openOrders` / `meta` calls, plus the
//! position PnL/entry fields the GUI surfaces).

pub use degenbox_signer_core::hl::info::HttpInfoClient;
