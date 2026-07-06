//! Hyperliquid signer subsystem — the single shared implementation.
//!
//! History: the HL keystore/signing/daemon stack used to exist in
//! triplicate (`hl-signer-desktop/src/*`, `signer-app/src-tauri/src/hl/*`,
//! and the keystore-only `signer-core/src/hl.rs`). This module is the
//! canonical merge: `signer-app` consumes it directly; `hl-signer-desktop`
//! (the live prod executor) keeps its own copy until its deprecation is
//! decided — semantics here follow that prod copy wherever the variants
//! diverged.
//!
//! Module map (everything except `keystore` is behind feature `hl-exec`):
//! - [`keystore`]   — encrypted on-disk agent key (Argon2id + AES-256-GCM),
//!   wire-compatible with the legacy Go bot and every signer front-end.
//! - [`config`]     — shared on-disk HL daemon config (`hl-config.json`),
//!   round-trips between the CLI and the desktop app.
//! - [`server`]     — gateway client (register / redeem / pending / result
//!   / verify-totp).
//! - [`info`]       — HL `/info` client (positions, balances, closedPnl,
//!   open orders, perp meta).
//! - [`signing`]    — payload → signed `/exchange` POST for every
//!   instruction kind.
//! - [`exec_state`] — restart-durable executed-marker (idempotency ledger).
//! - [`audit`]      — local append-only JSONL record of every sign.
//! - [`runtime`]    — shared live telemetry the daemon writes + UIs read.
//! - [`daemon`]     — the poll/sign/report loop (cursor + pause + TOTP),
//!   transport-agnostic via [`daemon::DaemonEvents`].

pub mod keystore;
pub use keystore::*;

#[cfg(feature = "hl-exec")]
pub mod audit;
#[cfg(feature = "hl-exec")]
pub mod config;
#[cfg(feature = "hl-exec")]
pub mod daemon;
#[cfg(feature = "hl-exec")]
pub mod exec_state;
#[cfg(feature = "hl-exec")]
pub mod info;
#[cfg(feature = "hl-exec")]
pub mod push;
#[cfg(feature = "hl-exec")]
pub mod runtime;
#[cfg(feature = "hl-exec")]
pub mod server;
#[cfg(feature = "hl-exec")]
pub mod signing;
