//! Shared HL daemon runtime telemetry — what the daemon writes and the
//! host UI (Tauri commands / TUI panels) reads.
//!
//! Deliberately UI-framework-free: the HL-specific live view of
//! connection phase, paired identity, balance snapshot, open positions,
//! queue depth, last error, and a pending TOTP challenge the UI must
//! answer. All `Mutex`-guarded so the daemon task and the host's IPC
//! layer can both touch it.

use chrono::{DateTime, Utc};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};

/// Coarse connection lifecycle the header pill + tray reflect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnState {
    /// No daemon running yet (locked / not registered).
    Offline,
    /// Registering / first poll in flight.
    Connecting,
    /// Registered, polling, healthy.
    Ready,
    /// Paused by the operator — heartbeat alive, not claiming/signing.
    Paused,
    /// Last poll/sign failed; retrying.
    Error,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct BalanceSnapshot {
    pub account_value_usd: Option<String>,
    pub withdrawable_usd: Option<String>,
    pub positions: Vec<PositionRow>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PositionRow {
    pub coin: String,
    pub szi: String,
    pub side: String,
    pub unrealized_pnl: Option<String>,
    pub entry_px: Option<String>,
}

/// A TOTP challenge the gateway raised on a `post_result`. The GUI shows
/// a code prompt and calls `submit_hl_totp`, which fills `answer`.
#[derive(Debug, Clone, Serialize)]
pub struct TotpPrompt {
    pub challenge_id: String,
    pub expires_at: String,
}

/// The live HL runtime, shared between the daemon task and IPC commands.
#[derive(Default)]
pub struct HlRuntime {
    pub conn: Mutex<Option<ConnState>>,
    pub user_id: Mutex<Option<String>>,
    pub discord_handle: Mutex<Option<String>>,
    pub agent_address: Mutex<Option<String>>,
    pub account_address: Mutex<Option<String>>,
    pub paper_mode: Mutex<bool>,
    pub balance: Mutex<BalanceSnapshot>,
    pub last_poll_at: Mutex<Option<DateTime<Utc>>>,
    pub queue_pending: Mutex<usize>,
    pub error: Mutex<Option<String>>,
    /// A pending TOTP challenge the GUI must answer (None when not blocked).
    pub totp_prompt: Mutex<Option<TotpPrompt>>,
    /// The code the GUI submitted, consumed by the daemon's TOTP wait.
    pub totp_answer: Mutex<Option<String>>,
    /// Daemon-running guard: set true when a daemon task is spawned so a
    /// second unlock doesn't race a duplicate poller against the queue.
    pub daemon_running: AtomicBool,
    /// Run generation. A spawner increments this AFTER winning the
    /// `daemon_running` CAS; the daemon loop captures the value at start
    /// and exits when it no longer matches. Closes the stop→respawn
    /// race: a stale loop that missed the brief `daemon_running=false`
    /// window (flag re-armed by the new spawn) still sees a generation
    /// mismatch and terminates instead of double-polling the queue.
    pub run_generation: AtomicU64,
}

pub type SharedHlRuntime = Arc<HlRuntime>;

impl HlRuntime {
    pub fn set_conn(&self, c: ConnState) {
        if let Ok(mut g) = self.conn.lock() {
            *g = Some(c);
        }
    }
    pub fn set_error(&self, msg: Option<String>) {
        if let Ok(mut g) = self.error.lock() {
            *g = msg;
        }
    }
}
