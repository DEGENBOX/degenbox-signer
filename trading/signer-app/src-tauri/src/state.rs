//! In-process state shared across IPC commands.
//!
//! We keep:
//!
//! - Decrypted HL agent secret (hex) — only after the user unlocks.
//!   Wrapped in a `Mutex<Option<…>>` so `lock_keystores` can swap it
//!   back to `None` and the bytes get dropped.
//! - Decrypted Solana keypair bytes — same pattern.
//! - Recent-signs ring buffer for the dashboard.
//! - Paused / status flag — surfaced to the tray icon thread.
//!
//! Concurrency: a single `tokio::sync::Mutex` is fine for the IPC
//! cadence (human-driven onboarding clicks + a per-second daemon
//! poll). No need for parking_lot or sharded locks.

use crate::hl::runtime::SharedHlRuntime;
use degenbox_signer_core::SignerSlot;
use serde::Serialize;
use std::sync::{Arc, Mutex};

/// Role a vault wallet plays in the multi-executor runtime topology.
///
/// Every unlocked, paired wallet runs its OWN executor now — the
/// gateway scopes HL claims per wallet (`?wallet=` on
/// `instructions/pending`) and stamps the executor identity on the
/// Solana sell/copy events (`wallet_pubkey`). The role only decides who
/// owns the LEGACY (unscoped/unstamped) work so it executes exactly
/// once:
///
/// - `Primary` (designated, default first of chain): additionally owns
///   unstamped HL rows + unstamped Sol events, mirrors into the legacy
///   single-wallet state fields, and serves the `:5829` slot (Sol).
/// - `Standby`: a strictly wallet-scoped executor — refuses unstamped
///   work. HL standbys fall back to register+balance-only mode when the
///   gateway predates wallet scoping (capability probe).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientRole {
    Primary,
    Standby,
}

/// One unlocked vault wallet ("client") + its live plumbing.
pub struct ClientHandle {
    /// Vault metadata snapshot (id, chain, address, label, paused).
    pub entry: degenbox_signer_core::WalletEntry,
    pub role: ClientRole,
    /// Effective pause gate = global kill-switch OR per-client pause.
    /// Recomputed by `clients::recompute_pause_gates` on every toggle;
    /// the HL daemon polls it inline on the money path, the Sol
    /// dispatcher reads it per event.
    pub pause_gate: Arc<Mutex<bool>>,
    /// Decrypted 32-byte Solana seed (`None` for HL wallets).
    pub sol_seed: Option<[u8; 32]>,
    /// Decrypted HL secp256k1 secret hex (`None` for Sol wallets).
    pub hl_secret_hex: Option<String>,
    /// Per-client HL telemetry. The primary HL wallet shares
    /// `AppState.hl_runtime` (back-compat for `hl_status`); other HL
    /// wallets get their own.
    pub hl_runtime: Option<SharedHlRuntime>,
    /// Per-client Sol engine telemetry. The primary Sol wallet shares
    /// `AppState.sol_runtime` (back-compat for `sol_runtime_status`);
    /// other Sol wallets get their own.
    pub sol_runtime: Option<crate::sol::runtime::SharedSolRuntime>,
    /// True while this HL wallet runs a FULL wallet-scoped daemon
    /// (poll/sign/report); false = standby fallback (register +
    /// balance only — unpaired, or the gateway predates wallet
    /// scoping). Drives the `executor:` vs `standby:` runtime label.
    pub hl_executor: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SignerHealth {
    /// Daemon running, recent successful sign or idle queue.
    Green,
    /// Recent retryable failure (server unreachable, HL 5xx, etc).
    Amber,
    /// Keystore locked, no agent registered, or fatal config error.
    Red,
}

impl Default for SignerHealth {
    fn default() -> Self {
        // Start red — we're locked until the user unlocks. The tray
        // icon and dashboard reflect this on first launch.
        Self::Red
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentSign {
    pub at: chrono::DateTime<chrono::Utc>,
    /// Which runtime produced the event — `"sol"` or `"hl"`. Lets the
    /// per-chain dashboards show only their own activity (additive
    /// field; the payload stays backward-compatible).
    pub chain: &'static str,
    pub kind: String,
    pub identifier: String,
    pub status: String,
}

#[derive(Default)]
pub struct AppState {
    /// Hex-encoded 32-byte HL secp256k1 secret. `None` when locked.
    pub hl_secret_hex: Arc<Mutex<Option<String>>>,
    /// 32-byte Solana ed25519 seed. `None` when locked.
    pub sol_seed: Arc<Mutex<Option<[u8; 32]>>>,
    /// Daemon paused — when true the background loop sleeps without
    /// polling the server. Shared straight into the HL daemon's pause
    /// gate so the UI toggle is the live control.
    pub paused: Arc<Mutex<bool>>,
    /// Live health flag the tray icon thread reads.
    pub health: Arc<Mutex<SignerHealth>>,
    /// Last 50 sign events for the dashboard table. The HL daemon pushes
    /// every handled instruction here.
    pub recent: Arc<Mutex<Vec<RecentSign>>>,
    /// Live HL daemon telemetry (connection, balance, positions, queue,
    /// TOTP prompt). Written by the daemon, read by the IPC commands.
    pub hl_runtime: SharedHlRuntime,
    /// Lockable Solana keypair slot shared with the `:5829`
    /// signer-protocol daemon. `unlock_keystores` installs the keypair,
    /// `lock_keystores` clears it — the HTTP server itself runs for the
    /// whole app lifetime so the web app's probe always answers.
    pub sol_slot: SignerSlot,
    /// Live status of the `:5829` daemon (running / port / bind error),
    /// surfaced via the `local_daemon_status` IPC command.
    pub local_daemon: Arc<Mutex<crate::local_daemon::LocalDaemonStatus>>,
    /// Handle to the `:5829` daemon's runtime config (gateway base +
    /// the auth token the web app pushes via `POST /setAuth`). The
    /// Solana gateway reads use it as the auth fallback for users who
    /// never paired an HL agent. `None` until the daemon spawns.
    #[allow(clippy::type_complexity)]
    pub web_auth: Arc<
        Mutex<Option<Arc<tokio::sync::RwLock<degenbox_signer_core::local_daemon::RuntimeConfig>>>>,
    >,
    /// Solana execution runtime (sell + copy stream consumers) —
    /// telemetry + stop handle. Spawned on unlock, stopped on lock.
    pub sol_runtime: crate::sol::runtime::SharedSolRuntime,
    /// Unlocked vault wallets ("clients"), in vault order. Empty while
    /// locked. The legacy single-wallet fields above mirror the
    /// designated primaries for backward compatibility.
    pub clients: Arc<Mutex<Vec<ClientHandle>>>,
    /// In-flight Discord desktop login (verifier waiting for its
    /// deep-link callback). Consumed exactly once by the exchange.
    pub discord_pending: Arc<Mutex<Option<crate::auth::PendingLogin>>>,
    /// Last Discord-login failure, user-readable. Cleared when a new
    /// login starts or one succeeds.
    pub discord_error: Arc<Mutex<Option<String>>>,
    /// Wallet addresses (lowercased) the user JUST added/imported on
    /// this device. The next gateway auto-register for them sends
    /// `revive: true` (one shot) so a deliberate re-add can resurrect a
    /// wallet that was removed from the account, while the background
    /// polling loop alone never can.
    pub revive_ok: Arc<Mutex<std::collections::HashSet<String>>>,
}

impl AppState {
    pub fn set_health(&self, h: SignerHealth) {
        if let Ok(mut guard) = self.health.lock() {
            *guard = h;
        }
    }

    pub fn push_recent(&self, ev: RecentSign) {
        if let Ok(mut guard) = self.recent.lock() {
            guard.insert(0, ev);
            // Bounded ring buffer — 50 is enough for the dashboard.
            if guard.len() > 50 {
                guard.truncate(50);
            }
        }
    }
}
