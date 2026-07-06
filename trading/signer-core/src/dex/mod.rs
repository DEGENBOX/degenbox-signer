//! Native DEX adapters — instruction encoders the signer uses to build
//! swap transactions without going through Jupiter.
//!
//! Each adapter is a pure-Rust module with:
//!   * A `build_buy_ix` / `build_sell_ix` that produces a `solana_sdk::
//!     instruction::Instruction` given the user pubkey, mint, and
//!     amount bounds.
//!   * Local helpers for the PDAs / ATAs the IDL requires.
//!   * Unit tests that match the discriminators + account ordering
//!     against the matching decoder in `platform-solana-tx`.
//!
//! Adapters never make HTTP calls — bonding-curve state, current
//! reserves, etc. are read by the caller (signer) via Solana RPC and
//! passed in. This keeps the adapter audit-friendly (no hidden
//! network) and lets the caller cache / batch state reads as they see
//! fit.

pub mod ata;
pub mod compute_budget;
pub mod pumpfun;
pub mod pumpfun_amm;
pub mod raydium_amm_v4;
