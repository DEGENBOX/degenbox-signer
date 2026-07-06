//! Hyperliquid `/exchange` client library.
//!
//! Pure transport + signing. No DB, no NATS, no orchestration. Callers
//! (in `module-hyperliquid::exchange`) compose this with persistence,
//! risk-gates and per-user agent-key resolution.
//!
//! ## Signing primer
//!
//! Hyperliquid uses **EIP-712 typed-data** signatures over a "phantom"
//! `Agent` struct. The wire flow per request is:
//!
//! 1. Build the action struct (e.g. `OrderAction`) and serialize to
//!    msgpack.
//! 2. Build `connectionId = keccak256(msgpack(action) ‖ nonce_be_u64 ‖
//!    vault_flag [‖ vault_addr])`.
//! 3. Build typed-data with `domain = {name: "Exchange", version: "1",
//!    chainId: 1337, verifyingContract: 0x0…0}` (chainId is ALWAYS
//!    1337 for the phantom agent regardless of mainnet/testnet — the
//!    `source` field discriminates: `"a"` for mainnet, `"b"` for
//!    testnet).
//! 4. Hash the typed-data with `keccak256(0x19 ‖ 0x01 ‖
//!    domain_separator ‖ message_hash)`.
//! 5. ECDSA-sign with secp256k1, return `{r, s, v}` where `v ∈ {27, 28}`.
//! 6. POST `{action, nonce, signature}` to `/exchange`.
//!
//! All of this is the reference behaviour of the Go signer used by the
//! legacy Hyperliquid bot in `legay-hyperliquid-bot/degenbox-client/`
//! and is the only signing path that HL accepts for L1 actions.

pub mod actions;
pub mod client;
pub mod cloid;
pub mod signer;
pub mod userfills;

pub use actions::{
    ApproveAgentAction, CancelAction, CancelByCloidAction, CancelByCloidSpec, CancelSpec, Grouping,
    LimitSpec, OrderAction, OrderType, OrderWire, TriggerWire, UpdateLeverageAction,
    VaultTransferAction,
};
pub use client::{
    ApprovalResult, CancelResult, ExchangeClient, ExchangeError, ExchangeResponse, OrderResult,
    OrderStatusEntry, Signature,
};
pub use cloid::new_cloid;
pub use signer::{AgentSigner, Network, SignerError};
pub use userfills::{parse_user_fills, sum_closed_pnl_for_oid, UserFill};

/// Estimated USD value for a market order given a quoted price. Used
/// by callers to pre-check size against per-user risk gates *before*
/// signing. Pure helper — the actual exchange may fill at a different
/// price.
pub fn usd_value_estimate(
    price: rust_decimal::Decimal,
    size: rust_decimal::Decimal,
) -> rust_decimal::Decimal {
    price * size
}
