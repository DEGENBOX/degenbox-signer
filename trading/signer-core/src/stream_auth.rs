//! Per-attempt credential resolution for the gateway push streams.
//!
//! ## Why this exists (audit H1)
//!
//! `spawn_sell_subscriber` / `spawn_copy_subscriber` used to capture a
//! single JWT `String` for the lifetime of their reconnect loops. The
//! gateway session JWT expires after ~24 h, so the first reconnect
//! after expiry would 401 forever at warn-level while the consumer's
//! status stayed "ready" — the bot was dead but reported green.
//!
//! The fix is two-sided and both sides live here:
//!
//! 1. **[`TokenProvider`]** — the streams ask for a FRESH credential on
//!    every reconnect attempt instead of reusing a captured string. The
//!    host (Tauri app / CLI) supplies a closure that re-runs its full
//!    auth-resolution chain (desktop-login JWT → HL pairing JWT →
//!    web-pushed session token), so a re-login or token refresh is
//!    picked up WITHOUT restarting the runtime.
//! 2. **[`StreamHealthSink`]** — the streams report auth-rejected
//!    upgrades (HTTP 401/403) and successful (re)subscribes so the host
//!    can flip its user-visible status to `auth_expired` / back to
//!    `ready` truthfully instead of guessing from silence.
//!
//! The legacy fixed-token spawn functions remain as thin wrappers over
//! [`fixed_token_provider`], so the CLI (`watch-sells` / `watch-copy`)
//! keeps compiling unchanged.

use futures_util::future::BoxFuture;
use std::sync::Arc;

/// Credentials for one websocket connect attempt. The base rides along
/// with the token because the host's resolution chain may source them
/// from different config files — they must never be mixed and matched.
#[derive(Debug, Clone)]
pub struct StreamAuth {
    /// Gateway origin, e.g. `https://api-v2.degenbox.app`.
    pub gateway_base: String,
    /// Bearer JWT for the `/ws?token=` upgrade.
    pub token: String,
}

/// Async source of fresh stream credentials, called once per connect
/// attempt. `Err` means "no usable credential right now" (everything
/// expired / signed out) — the stream keeps retrying and reports
/// [`StreamHealth::AuthFailed`] so the host can surface it.
pub type TokenProvider =
    Arc<dyn Fn() -> BoxFuture<'static, Result<StreamAuth, String>> + Send + Sync>;

/// Connection-health events the streams emit (best-effort, fire and
/// forget). Hosts use these to drive truthful status UI; ignoring them
/// (`None` sink) preserves the legacy warn-log-only behaviour.
#[derive(Debug, Clone)]
pub enum StreamHealth {
    /// The websocket is up and the subject subscription was sent.
    Subscribed,
    /// The gateway refused the credentials: either the upgrade was
    /// rejected with HTTP 401/403, or the [`TokenProvider`] itself
    /// could not produce a token. The stream keeps retrying — a
    /// re-login is picked up on the next attempt.
    AuthFailed { message: String },
    /// Any other connect/stream failure (network blip, gateway
    /// restart). Transient; the reconnect loop handles it.
    Disconnected { message: String },
}

/// Callback the host registers to observe [`StreamHealth`] events.
pub type StreamHealthSink = Arc<dyn Fn(StreamHealth) + Send + Sync>;

/// Wrap a fixed `(gateway_base, token)` pair as a [`TokenProvider`] —
/// the legacy single-credential behaviour for CLI consumers whose
/// token lives exactly as long as the process.
pub fn fixed_token_provider(gateway_base: String, token: String) -> TokenProvider {
    Arc::new(move || {
        let auth = StreamAuth {
            gateway_base: gateway_base.clone(),
            token: token.clone(),
        };
        Box::pin(async move { Ok(auth) })
    })
}

/// Extract the HTTP status from a tungstenite connect error, when the
/// failure was a rejected upgrade (the gateway's `/ws` route returns
/// plain `401 Unauthorized` for a bad/expired token — see
/// `platform-ws::lib.rs`). `None` for transport-level failures.
pub fn http_reject_status(e: &tokio_tungstenite::tungstenite::Error) -> Option<u16> {
    match e {
        tokio_tungstenite::tungstenite::Error::Http(resp) => Some(resp.status().as_u16()),
        _ => None,
    }
}

/// Is this upgrade-rejection status an auth failure (401/403)?
pub fn is_auth_status(status: u16) -> bool {
    status == 401 || status == 403
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixed_provider_returns_the_same_credentials_every_call() {
        let p = fixed_token_provider("https://gw".into(), "tok-1".into());
        for _ in 0..3 {
            let a = p().await.expect("fixed provider never fails");
            assert_eq!(a.gateway_base, "https://gw");
            assert_eq!(a.token, "tok-1");
        }
    }

    #[test]
    fn http_reject_status_classifies_upgrade_rejections() {
        use tokio_tungstenite::tungstenite::Error;
        let resp = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(401)
            .body(None)
            .unwrap();
        assert_eq!(http_reject_status(&Error::Http(resp)), Some(401));
        // Transport errors are not auth rejections.
        assert_eq!(
            http_reject_status(&Error::Url(
                tokio_tungstenite::tungstenite::error::UrlError::NoHostName
            )),
            None
        );
    }

    #[test]
    fn auth_statuses_are_401_and_403_only() {
        assert!(is_auth_status(401));
        assert!(is_auth_status(403));
        assert!(!is_auth_status(400));
        assert!(!is_auth_status(500));
        assert!(!is_auth_status(200));
    }
}
