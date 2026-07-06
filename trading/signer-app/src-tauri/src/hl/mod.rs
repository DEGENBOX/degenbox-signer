//! Hyperliquid signing subsystem for the desktop app.
//!
//! Since the Wave-4 dedupe this is a THIN consumer of
//! `degenbox-signer-core`'s `hl` module — the keystore, config, gateway
//! client, `/info` client, payload signing, executed-marker ledger,
//! audit log, runtime telemetry, and the poll/sign/report daemon core
//! all live there (single source of truth, shared with the CLI
//! lineage). The only app-local piece is [`daemon`], a small adapter
//! that bridges the core daemon's events into Tauri state
//! (recent-signs ring + tray health).

// Re-export the core modules the app's command layer touches; anything
// else (signing, info, exec_state, audit) is reachable via
// `degenbox_signer_core::hl::*` directly.
pub use degenbox_signer_core::hl::{config, runtime, server};

pub mod daemon;
