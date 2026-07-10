//! DegenBox signer core.
//!
//! Self-custody Solana signer. Two transports build on this:
//!   * `signer-extension` — Chrome MV3 service worker, WASM build
//!   * `signer-desktop`   — Tauri shell + localhost daemon
//!
//! The library exposes pure functions; transports wrap them into the
//! frozen `signer-protocol` RPC surface.
//!
//! ## Slice T.1 — what's in this build
//!
//! - `keystore`  — Argon2id + AES-GCM encrypted keypair blob
//! - `jupiter`   — Jupiter Swap API (`/swap/v1`) quote + swap client
//! - `signer`    — sign Jupiter-supplied `VersionedTransaction`
//! - `relay`     — gateway client (`/api/trading/intents` create + submit)
//!
//! ## What's NEXT (T.2+)
//!
//! - simulator: run `simulateTransaction` before sign to catch bad
//!   routes before they hit chain
//! - program-allow: enforce whitelist of program ids
//! - bot-engine: signal-feed consumer, preset-trigger evaluator
//! - budget-guard: per-session / per-token / per-hour caps
//! - wasm target: feature `wasm` strips solana-sdk + replaces with an
//!   in-tree v0 message encoder
//!
//! ## Threat model
//!
//! - Keys decrypt to a `Keypair` only inside the signer process.
//! - Argon2id-derived AES-GCM resists offline brute-force of a
//!   leaked keystore file.
//! - Decrypted secret is zeroized immediately after the keypair is
//!   constructed; window of in-RAM exposure is bounded.
//! - The signer NEVER trusts the gateway: `/swap` reads from Jupiter
//!   directly, fee-payer is verified to match our pubkey before
//!   signing, slippage comes from the user not the relay.

#![deny(unsafe_code)]
#![warn(missing_docs)]
#![allow(missing_docs)]

pub mod ata_cache;
pub mod blockhash_cache;
pub mod bot_engine;
pub mod budget;
pub mod copy_stream;
pub mod dex;
pub mod fee_strategy;
pub mod hl;
pub mod intent_stream;
pub mod jupiter;
pub mod keystore;
#[cfg(feature = "localhost-daemon")]
pub mod local_daemon;
pub mod os_keychain;
pub mod paths;
pub mod program_allow;
pub mod register;
pub mod relay;
pub mod route;
pub mod rpc;
pub mod sell_stream;
pub mod signer;
pub mod simulator;
pub mod stream_auth;
pub mod update;
pub mod vault;
pub mod ws_stream;

pub use hl::{
    derive_address as hl_derive_address, load as hl_load_keystore, peek_address as hl_peek_address,
    save as hl_save_keystore, HlKeystore, HlKeystoreError,
};
#[cfg(feature = "localhost-daemon")]
pub use local_daemon::{
    default_port as local_daemon_default_port, serve as serve_local_daemon,
    DaemonState as LocalDaemonState, SignerSlot,
};
pub use os_keychain::{KeystoreBackend, OsKeychainError};
pub use paths::{
    app_log_path, default_dir, hl_config_path, hl_keystore_path, sol_keystore_path, PathsError,
};

/// Crate version surfaced over the RPC for clients that want to gate features.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use ata_cache::AtaCache;
pub use blockhash_cache::BlockhashCache;
pub use bot_engine::{BotConfig, BotEngine, BotError, BotStats, Decision, PresetMatcher, Signal};
pub use budget::{BudgetConfig, BudgetError, BudgetState};
pub use copy_stream::{
    cash_denominator_lamports, resolve_buy_lamports, spawn_copy_subscriber,
    spawn_copy_subscriber_with, wallet_event_is_mine, CopyExecEvent, CopyStreamError,
};
pub use fee_strategy::{
    spawn_priority_fee_poller, DexId, FeeParams, FeeStrategy, FeeTier, Side as TradeSide,
};
pub use intent_stream::{
    spawn_intent_subscriber, spawn_intent_subscriber_with, IntentStreamError, ManualIntentEvent,
};
pub use jupiter::{JupiterClient, JupiterError, QuoteResponse, SwapOptions, SwapResponse};
pub use keystore::{
    decrypt, encrypt, generate, import_extension_json, load_from_path, save_to_path, Keystore,
    KeystoreError,
};
pub use program_allow::{default_allowlist, Allowlist, AllowlistError};
pub use relay::{
    CreateIntentReq, IntentRow, OrderSummary, PresetMatchRow, RelayClient, RelayError, SubmitResp,
};
#[allow(deprecated)]
pub use route::{select_for_buy, select_for_token, RouteError, SwapRoute};
pub use rpc::{gateway_proxy_rpc_url, RpcClient, RpcError};
pub use sell_stream::{
    spawn_sell_subscriber, spawn_sell_subscriber_with, SellNeededEvent, SellStreamError,
    TriggerKind,
};
pub use signer::{sign_jupiter_tx_b64, sign_versioned_tx_bytes, SignError};
pub use simulator::{SimulateError, SimulationOutcome, Simulator};
pub use stream_auth::{
    fixed_token_provider, StreamAuth, StreamHealth, StreamHealthSink, TokenProvider,
};
pub use vault::{
    default_vault_dir, MigrationReport, Vault, VaultError, VaultMeta, WalletChain, WalletEntry,
};
pub use ws_stream::{spawn_subscriber, WsStreamError};

// Solana types re-exported so consumers don't have to depend on
// solana-sdk directly. Keeping the surface narrow — only the user-
// facing keypair + signer trait + the tx type that callers need to
// decode/inspect before passing back into `Allowlist::check_tx`.
pub use solana_sdk::signature::{Keypair, Signer};
pub use solana_sdk::transaction::VersionedTransaction;

/// Decode a Jupiter-supplied base64 unsigned transaction into a
/// `VersionedTransaction`. Useful for callers that want to inspect
/// the tx (e.g. with `Allowlist::check_tx`) before passing it to
/// `sign_jupiter_tx_b64`. Wraps the bincode + base64 dance so CLIs
/// don't have to depend on either crate.
pub fn decode_jupiter_tx_b64(b64: &str) -> Result<VersionedTransaction, SignError> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| SignError::Base64(e.to_string()))?;
    bincode::deserialize(&raw).map_err(|e| SignError::Bincode(e.to_string()))
}
