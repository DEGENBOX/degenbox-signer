//! Solana panel state for the interactive TUI — the in-process
//! equivalent of `sol daemon`: unlock arms the `SignerSlot`, starts the
//! `127.0.0.1:5829` signer-protocol daemon (web-app detection) and the
//! sell+copy execution runtime. All state is shared handles the Solana
//! tab renders each tick.

use crate::sol::config::SolConfig;
use crate::sol::runtime::{self, AuthSource, SharedSolRuntime, SolRuntimeInner, SpawnArgs};
use degenbox_signer_core::{local_daemon::RuntimeConfig, Signer as _, SignerSlot};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct SolPanel {
    /// Resolved keystore path (shared location, falling back to the
    /// legacy signer-cli file). `None` when no keystore exists yet.
    pub ks_path: Option<PathBuf>,
    pub pubkey: Option<String>,
    /// Last unlock error (wrong password, corrupt file …).
    pub unlock_error: Option<String>,
    /// Lockable keypair slot shared with the `:5829` daemon.
    pub slot: SignerSlot,
    /// Execution-runtime telemetry + activity ring.
    pub runtime: SharedSolRuntime,
    /// `:5829` signer-protocol daemon state.
    pub daemon_started: bool,
    pub daemon_port: u16,
    pub daemon_alive: Arc<AtomicBool>,
    pub daemon_error: Arc<Mutex<Option<String>>>,
    /// The daemon's live gateway/auth config (web app pushes a session
    /// token via /setAuth) — the runtime's auth fallback.
    web_config: Option<Arc<tokio::sync::RwLock<RuntimeConfig>>>,
}

impl SolPanel {
    pub fn bootstrap() -> Self {
        let ks_path = crate::sol::resolve_keystore_path(None).ok();
        let pubkey = ks_path.as_deref().and_then(crate::sol::peek_pubkey);
        Self {
            ks_path,
            pubkey,
            unlock_error: None,
            slot: SignerSlot::default(),
            runtime: Arc::new(SolRuntimeInner::default()),
            daemon_started: false,
            daemon_port: degenbox_signer_core::local_daemon_default_port(),
            daemon_alive: Arc::new(AtomicBool::new(false)),
            daemon_error: Arc::new(Mutex::new(None)),
            web_config: None,
        }
    }

    /// Inert panel for snapshot tests (no keystore, nothing running).
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            ks_path: None,
            pubkey: None,
            unlock_error: None,
            slot: SignerSlot::default(),
            runtime: Arc::new(SolRuntimeInner::default()),
            daemon_started: false,
            daemon_port: degenbox_signer_core::local_daemon_default_port(),
            daemon_alive: Arc::new(AtomicBool::new(false)),
            daemon_error: Arc::new(Mutex::new(None)),
            web_config: None,
        }
    }

    pub fn has_keystore(&self) -> bool {
        self.ks_path.is_some()
    }

    pub fn is_unlocked(&self) -> bool {
        self.slot.unlocked().is_some()
    }

    /// Decrypt the keystore, arm the slot, start the `:5829` daemon
    /// (once) and (re)start the execution runtime. Returns a friendly
    /// error string for the modal on failure.
    pub fn unlock(&mut self, password: &str) -> Result<(), String> {
        let Some(path) = self.ks_path.clone() else {
            return Err("no Solana keystore — run `hl-signer-desktop sol init` first".into());
        };
        let kp = degenbox_signer_core::load_from_path(&path, password).map_err(|e| match e {
            degenbox_signer_core::KeystoreError::BadPassword => "wrong password".to_string(),
            other => other.to_string(),
        })?;
        self.pubkey = Some(kp.pubkey().to_string());
        self.unlock_error = None;

        // Arm the lockable slot (a second copy stays with the runtime).
        let runtime_kp = degenbox_signer_core::Keypair::try_from(&kp.to_bytes()[..])
            .map_err(|e| format!("keypair clone: {e}"))?;
        self.slot.install(kp);

        self.ensure_daemon();
        self.respawn_runtime(Arc::new(runtime_kp));
        Ok(())
    }

    /// Start the `:5829` signer-protocol daemon exactly once for the TUI
    /// session. Bind failures (port in use — e.g. a `signer-cli daemon`
    /// still running) surface on the panel, never crash the TUI.
    fn ensure_daemon(&mut self) {
        if self.daemon_started {
            return;
        }
        self.daemon_started = true;
        // Gateway base: the shared HL config's server URL (defaults to
        // the v2 gateway); the web app overrides via /setGateway.
        let gateway = crate::config::default_config_path()
            .ok()
            .and_then(|p| crate::config::load(&p).ok())
            .map(|c| c.server_url)
            .unwrap_or_else(|| crate::config::Config::default().server_url);
        let rpc_url = SolConfig::load_or_default().resolved_rpc_url();
        let state =
            degenbox_signer_core::LocalDaemonState::new(self.slot.clone(), gateway, rpc_url)
                .with_client_kind("hl-signer-desktop");
        self.web_config = Some(state.config.clone());
        let port = self.daemon_port;
        let alive = self.daemon_alive.clone();
        let error = self.daemon_error.clone();
        tokio::spawn(async move {
            alive.store(true, Ordering::Relaxed);
            let r = degenbox_signer_core::serve_local_daemon(state, port).await;
            alive.store(false, Ordering::Relaxed);
            if let Err(e) = r {
                tracing::error!(error = %e, "local signer-protocol daemon stopped");
                if let Ok(mut g) = error.lock() {
                    *g = Some(format!("{e:#}"));
                }
            }
        });
    }

    /// (Re)start the execution runtime with the current `sol-config.json`
    /// (budget changes restart-safe — the old loop is stopped first).
    pub fn respawn_runtime(&self, kp: Arc<degenbox_signer_core::Keypair>) {
        runtime::spawn(
            self.runtime.clone(),
            SpawnArgs {
                kp,
                auth: AuthSource::Auto {
                    web: self.web_config.clone(),
                },
                cfg: SolConfig::load_or_default(),
                stdout_log: false,
            },
        );
    }

    /// Apply a new copy-session budget: persist + live-restart the
    /// runtime when unlocked (same semantics as the desktop app's
    /// Settings save).
    pub fn set_budget(&self, session_sol: Option<f64>) -> Result<(), String> {
        let mut cfg = SolConfig::load_or_default();
        cfg.copy_session_sol = session_sol.filter(|s| s.is_finite() && *s > 0.0);
        cfg.save()?;
        if let Some(kp) = self.slot.unlocked() {
            // The slot holds an Arc; the runtime pins its own clone.
            let kp2 = degenbox_signer_core::Keypair::try_from(&kp.to_bytes()[..])
                .map_err(|e| format!("keypair clone: {e}"))?;
            self.respawn_runtime(Arc::new(kp2));
        }
        Ok(())
    }

    /// Lock: stop the runtime + clear the slot (the `:5829` daemon keeps
    /// serving /health and answers 503 on signing routes).
    pub fn lock(&mut self) {
        self.runtime.stop();
        self.slot.clear();
    }
}
