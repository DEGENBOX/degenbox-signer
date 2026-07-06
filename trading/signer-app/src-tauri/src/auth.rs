//! Discord desktop-auth: PKCE-style browser hand-off against the
//! DegenBox gateway.
//!
//! Flow (frozen contract with the gateway slice):
//!
//! 1. `discord_login_start` generates a random 32-byte `verifier`
//!    (base64url), stores it as the pending login, and opens the system
//!    browser at
//!    `{gateway}/api/auth/discord/start?flow=desktop&challenge=<base64url(sha256(verifier))>`.
//! 2. After the user authorizes on Discord, the gateway redirects the
//!    browser to the deep link `degenbox://auth/callback?code=<one-time>`
//!    (or `?error=<code>`), which lands here via tauri-plugin-deep-link
//!    (macOS) or the single-instance argv forward (Windows/Linux).
//! 3. We `POST {gateway}/api/auth/desktop/exchange {"code","verifier"}`
//!    → `{"token","expires_at","discord":{"id","username","avatar"}}`
//!    and persist the result to `desktop-auth.json` (0600).
//!
//! The minted JWT then feeds BOTH runtimes automatically:
//!
//! - Solana: `sol::gateway::resolve_auth` reads it FIRST (before the HL
//!   pairing token and the web-app `/setAuth` push), and the `:5829`
//!   daemon's `RuntimeConfig.auth_token` is seeded with it so web-app
//!   swaps relay without a manual token push.
//! - Hyperliquid: `hl_pair` falls back to it as the bearer token when
//!   the user pastes no connect token — pairing becomes one click for a
//!   linked account.
//!
//! Until the gateway side ships, the exchange 404s — that's surfaced as
//! a readable "Anmeldung fehlgeschlagen" error on the Account page, the
//! UI never dead-ends.

use crate::state::AppState;
use base64::Engine as _;
use degenbox_signer_core as core;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::PathBuf;
use tauri::Manager;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// How long a started login stays redeemable before we treat the
/// callback as stale (browser tab left open for hours, etc).
const PENDING_TTL_SECS: u64 = 10 * 60;

/// Slack allowed on the client-side JWT-expiry check to tolerate a local
/// clock that runs fast. Only affects whether WE pre-emptively drop a
/// token; the gateway remains the authoritative verifier.
const CLOCK_SKEW_GRACE: chrono::Duration = chrono::Duration::minutes(5);

// ─── persisted account ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordProfile {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub avatar: Option<String>,
}

/// On-disk shape of `desktop-auth.json` — the Discord-minted gateway
/// JWT plus the profile for the linked-account card. 0600, atomic
/// writes, same directory as the keystores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopAuth {
    pub token: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    pub discord: DiscordProfile,
    /// Gateway base the token was minted against — auth reads must hit
    /// the same host.
    #[serde(default = "default_gateway")]
    pub gateway_base: String,
}

fn default_gateway() -> String {
    "https://api-v2.degenbox.app".into()
}

fn auth_path() -> Result<PathBuf, String> {
    Ok(core::default_dir()
        .map_err(|e| e.to_string())?
        .join("desktop-auth.json"))
}

impl DesktopAuth {
    pub fn load() -> Option<Self> {
        let path = auth_path().ok()?;
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// True when `expires_at` is present, parseable and comfortably in
    /// the past. Unparseable / absent expiries are treated as still-valid
    /// — the gateway rejects a genuinely dead token anyway, with a
    /// clearer error than a silent client-side drop.
    ///
    /// A `CLOCK_SKEW_GRACE` slack absorbs a client whose clock runs a
    /// little fast: without it, a freshly-minted token can read as
    /// "already expired" the instant it lands (seen on a Mac with a
    /// skewed clock — constant "session expired, re-link" right after a
    /// successful re-link, while the same account worked from a
    /// correct-clock host). This is purely a "don't self-drop what is
    /// probably still good" filter; the gateway stays the real verifier.
    pub fn expired(&self) -> bool {
        self.expires_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|t| t + CLOCK_SKEW_GRACE < chrono::Utc::now())
    }

    /// Load and filter to a non-expired account in one step.
    pub fn load_valid() -> Option<Self> {
        Self::load().filter(|a| !a.expired())
    }

    pub fn save(&self) -> Result<(), String> {
        let path = auth_path()?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        tmp.write_all(&bytes).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600));
        }
        tmp.persist(&path).map_err(|e| e.error.to_string())?;
        Ok(())
    }

    pub fn clear() -> Result<(), String> {
        let path = auth_path()?;
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

// ─── in-flight login ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub verifier: String,
    pub started_at: std::time::Instant,
    pub gateway_base: String,
}

impl PendingLogin {
    pub fn stale(&self) -> bool {
        self.started_at.elapsed().as_secs() > PENDING_TTL_SECS
    }
}

fn set_error(state: &AppState, err: Option<String>) {
    if let Ok(mut g) = state.discord_error.lock() {
        *g = err;
    }
}

/// Generate verifier + challenge and open the system browser at the
/// gateway's Discord-start route. The pending verifier replaces any
/// prior one — only the LATEST started login can complete.
pub fn start_login(app: &tauri::AppHandle, server_url: Option<String>) -> Result<(), String> {
    let state = app.state::<AppState>();
    let base = server_url
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            crate::hl::config::HlConfig::load_or_default()
                .server_url
                .trim_end_matches('/')
                .to_string()
        });

    let mut raw = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    let verifier = B64.encode(raw);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));

    if let Ok(mut g) = state.discord_pending.lock() {
        *g = Some(PendingLogin {
            verifier,
            started_at: std::time::Instant::now(),
            gateway_base: base.clone(),
        });
    }
    set_error(&state, None);

    let url = format!("{base}/api/auth/discord/start?flow=desktop&challenge={challenge}");
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("could not open the browser: {e}"))
}

// ─── deep-link callback ─────────────────────────────────────────────

/// Handle any `degenbox://` deep link. Auth callbacks are consumed
/// here; everything else is ignored (the `/hl/setup` return link is a
/// plain "bring the window to front" today).
pub fn handle_deep_link(app: &tauri::AppHandle, url: &str) {
    // Always surface the window — the user just came from the browser.
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
    let Ok(parsed) = tauri::Url::parse(url) else {
        tracing::warn!(%url, "unparseable deep link ignored");
        return;
    };
    if parsed.scheme() != "degenbox" {
        return;
    }
    // degenbox://auth/callback → host = "auth", path = "/callback".
    let is_auth_callback =
        parsed.host_str() == Some("auth") && parsed.path().trim_end_matches('/') == "/callback";
    if !is_auth_callback {
        tracing::info!(%url, "non-auth deep link — window focused only");
        return;
    }

    let mut code: Option<String> = None;
    let mut error: Option<String> = None;
    for (k, v) in parsed.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }

    let state = app.state::<AppState>();
    if let Some(err) = error {
        tracing::warn!(%err, "discord auth callback returned an error");
        set_error(
            &state,
            Some(format!("Anmeldung fehlgeschlagen — Discord meldet: {err}")),
        );
        if let Ok(mut g) = state.discord_pending.lock() {
            *g = None;
        }
        return;
    }
    let Some(code) = code else {
        set_error(
            &state,
            Some("Anmeldung fehlgeschlagen — der Callback enthielt keinen Code.".into()),
        );
        return;
    };

    // Take (consume) the pending verifier — a one-time code must never
    // be replayable against a second exchange.
    let pending = state.discord_pending.lock().ok().and_then(|mut g| g.take());
    let Some(pending) = pending else {
        set_error(
            &state,
            Some(
                "Anmeldung fehlgeschlagen — kein laufender Login. \
                 Starte die Verbindung erneut über den Account-Tab."
                    .into(),
            ),
        );
        return;
    };
    if pending.stale() {
        set_error(
            &state,
            Some(
                "Anmeldung fehlgeschlagen — der Login ist abgelaufen. Bitte erneut starten.".into(),
            ),
        );
        return;
    }

    let app2 = app.clone();
    // tauri::async_runtime, NOT tokio::spawn: this is invoked from sync
    // contexts (deep-link callback / single-instance handler) with no
    // tokio context — a raw tokio::spawn SIGABRTs the app (see 4f9c07c).
    tauri::async_runtime::spawn(async move {
        complete_exchange(app2, pending, code).await;
    });
}

#[derive(Debug, Serialize)]
struct ExchangeReq<'a> {
    code: &'a str,
    verifier: &'a str,
}

#[derive(Debug, Deserialize)]
struct ExchangeResp {
    token: String,
    #[serde(default)]
    expires_at: Option<String>,
    discord: DiscordProfile,
}

async fn complete_exchange(app: tauri::AppHandle, pending: PendingLogin, code: String) {
    let state = app.state::<AppState>();
    let url = format!("{}/api/auth/desktop/exchange", pending.gateway_base);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("reqwest client");
    let res = client
        .post(&url)
        .json(&ExchangeReq {
            code: &code,
            verifier: &pending.verifier,
        })
        .send()
        .await;

    let resp = match res {
        Ok(r) => r,
        Err(e) => {
            set_error(
                &state,
                Some(format!(
                    "Anmeldung fehlgeschlagen — Gateway nicht erreichbar: {e}"
                )),
            );
            return;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let msg = if status.as_u16() == 404 {
            // Backend slice not deployed yet — graceful, actionable.
            "Anmeldung fehlgeschlagen — der Server unterstützt den Desktop-Login \
             noch nicht. Bitte später erneut versuchen oder das Pairing über \
             einen Connect-Token nutzen."
                .to_string()
        } else {
            format!("Anmeldung fehlgeschlagen — Gateway {status}: {body}")
        };
        tracing::warn!(%status, "discord desktop exchange failed");
        set_error(&state, Some(msg));
        return;
    }
    let parsed: ExchangeResp = match resp.json().await {
        Ok(p) => p,
        Err(e) => {
            set_error(
                &state,
                Some(format!("Anmeldung fehlgeschlagen — Antwort unlesbar: {e}")),
            );
            return;
        }
    };

    let auth = DesktopAuth {
        token: parsed.token,
        expires_at: parsed.expires_at,
        discord: parsed.discord,
        gateway_base: pending.gateway_base,
    };
    if let Err(e) = auth.save() {
        set_error(
            &state,
            Some(format!(
                "Anmeldung fehlgeschlagen — konnte nicht speichern: {e}"
            )),
        );
        return;
    }
    set_error(&state, None);
    tracing::info!(user = %auth.discord.username, "discord account linked");

    // Feed the :5829 daemon so web-app swaps authenticate without a
    // manual /setAuth push, then nudge the Solana runtime so a
    // `waiting_auth` loop picks the token up immediately.
    install_runtime_token(&app, &auth.token).await;
    crate::sol::runtime::spawn(&app);
}

/// Push the Discord-minted JWT into the `:5829` daemon's runtime
/// config. Called after login and on boot (when a persisted account
/// exists).
pub async fn install_runtime_token(app: &tauri::AppHandle, token: &str) {
    let state = app.state::<AppState>();
    let rc = state.web_auth.lock().ok().and_then(|g| g.as_ref().cloned());
    if let Some(rc) = rc {
        rc.write().await.auth_token = Some(token.to_string());
    }
}

// ─── IPC commands ───────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DiscordStatus {
    pub linked: bool,
    /// A login was started and its callback hasn't arrived yet.
    pub pending: bool,
    pub discord_id: Option<String>,
    pub username: Option<String>,
    pub avatar: Option<String>,
    pub expires_at: Option<String>,
    pub expired: bool,
    pub gateway: Option<String>,
    /// Last login failure, user-readable. Cleared on the next start.
    pub error: Option<String>,
}

#[tauri::command]
pub fn discord_login_start(
    server_url: Option<String>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    start_login(&app, server_url)
}

#[tauri::command]
pub fn discord_account_status(state: tauri::State<'_, AppState>) -> Result<DiscordStatus, String> {
    let auth = DesktopAuth::load();
    let pending = state
        .discord_pending
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .is_some_and(|p| !p.stale());
    let error = state.discord_error.lock().ok().and_then(|g| g.clone());
    Ok(match auth {
        Some(a) => DiscordStatus {
            linked: true,
            pending,
            expired: a.expired(),
            discord_id: Some(a.discord.id),
            username: Some(a.discord.username),
            avatar: a.discord.avatar,
            expires_at: a.expires_at,
            gateway: Some(a.gateway_base),
            error,
        },
        None => DiscordStatus {
            linked: false,
            pending,
            expired: false,
            discord_id: None,
            username: None,
            avatar: None,
            expires_at: None,
            gateway: None,
            error,
        },
    })
}

#[tauri::command]
pub async fn discord_unlink(
    state: tauri::State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let ours = DesktopAuth::load().map(|a| a.token);
    DesktopAuth::clear()?;
    if let Ok(mut g) = state.discord_pending.lock() {
        *g = None;
    }
    set_error(&state, None);
    // Drop the token from the :5829 daemon ONLY if it is the one we
    // installed — a session token the web app pushed stays untouched.
    if let Some(ours) = ours {
        let rc = state.web_auth.lock().ok().and_then(|g| g.as_ref().cloned());
        if let Some(rc) = rc {
            let mut g = rc.write().await;
            if g.auth_token.as_deref() == Some(ours.as_str()) {
                g.auth_token = None;
            }
        }
    }
    // The Solana runtime may have been riding this token — restart so
    // it re-resolves (and visibly waits for credentials if none left).
    crate::sol::runtime::spawn(&app);
    Ok(())
}

// ─── access / subscription watch (W1 unlock-UX) ─────────────────────
//
// The locked decision is "unlock once per start, holds until app-close
// or sub-/access-loss". The loss half needs a *live* signal: the shell
// polls this command (~5 min + on the Sol runtime's 401 flag) and calls
// `lock_keystores` when the gateway actively rejects our credentials.
//
// No new backend endpoint was invented: this rides `GET /auth/me` —
// the exact probe the Sol runtime already uses (`RelayClient::
// fetch_user_id`, signer-core/src/relay.rs) — with the same credential
// resolution chain (`sol::gateway::resolve_auth`: desktop JWT → HL
// pairing JWT → web-pushed session token). A 401/403 there is the
// gateway's authoritative "this user no longer has access" (expired
// session, revoked token, sub-gated claims rejected). Network errors
// and 5xx are deliberately NOT access loss — never lock a trader out
// because the wifi blipped.

/// This user's Solana paper/live verdict, mirrored from the gateway's
/// `GET /api/trading/sol-mode` (`module-trading::api::sol_mode::
/// SolModeView`). `effective_live == false` ⇒ every Solana trade this
/// account fires goes through the non-broadcasting stub — the shell
/// shows the same "paper" badge the HL side already has.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolModeDto {
    pub user_live: bool,
    pub global_live: bool,
    pub effective_live: bool,
}

/// Result of one gateway access probe.
#[derive(Debug, Serialize)]
pub struct AccessCheck {
    /// `ok` — credentials accepted; `no_auth` — nothing to check (the
    /// device was never linked/paired, or the persisted token already
    /// expired client-side); `revoked` — the gateway answered 401/403
    /// (lock!); `unreachable` — network/5xx, verdict unknown (do NOT
    /// lock).
    pub state: &'static str,
    pub detail: Option<String>,
    /// The raw `/auth/me` payload when `state == "ok"` — serialized
    /// `AuthClaims` (`sub`, `discord_handle`, `roles`, `exp`, …). The
    /// Account tab reads roles/expiry from here; shape stays the
    /// gateway's, the GUI reads fields defensively.
    pub me: Option<serde_json::Value>,
    /// Solana paper/live state, piggybacked on the SAME probe request
    /// cycle (UX-honesty wave: the shell needs to badge paper mode but
    /// must not grow a new polling loop). Best-effort: `None` when the
    /// fetch fails or the route is gated — the badge simply stays off.
    pub sol_mode: Option<SolModeDto>,
}

#[tauri::command]
pub async fn access_check(state: tauri::State<'_, AppState>) -> Result<AccessCheck, String> {
    let auth = match crate::sol::gateway::resolve_auth(&state).await {
        Ok(a) => a,
        Err(e) => {
            return Ok(AccessCheck {
                state: "no_auth",
                detail: Some(e),
                me: None,
                sol_mode: None,
            })
        }
    };
    let url = format!("{}/auth/me", auth.base);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = match client.get(&url).bearer_auth(&auth.token).send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok(AccessCheck {
                state: "unreachable",
                detail: Some(format!("GET /auth/me: {e}")),
                me: None,
                sol_mode: None,
            })
        }
    };
    let status = resp.status();
    if status.is_success() {
        // Body = serialized AuthClaims; pass it through so the Account
        // tab can show roles/expiry. A decode failure is not an access
        // problem — credentials were accepted.
        let me = resp.json::<serde_json::Value>().await.ok();
        // Piggyback the Solana paper/live state on the same probe so
        // the shell can badge paper mode without a new polling loop.
        // Best-effort: any failure (network, non-2xx, decode) just
        // leaves the badge off — never affects the access verdict.
        let sol_mode = match client
            .get(format!("{}/api/trading/sol-mode", auth.base))
            .bearer_auth(&auth.token)
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => r.json::<SolModeDto>().await.ok(),
            _ => None,
        };
        return Ok(AccessCheck {
            state: "ok",
            detail: None,
            me,
            sol_mode,
        });
    }
    let body = resp.text().await.unwrap_or_default();
    let code = status.as_u16();
    if code == 401 && body.contains("ExpiredSignature") {
        // A stale token is the credential's normal lifecycle, not an
        // access revocation — surface "re-login", never lock. (The
        // resolve chain filters client-side-known-expired tokens; this
        // catches clock skew and gateway-side-only expiry.)
        tracing::warn!("access probe: token expired — re-login required");
        return Ok(AccessCheck {
            state: "no_auth",
            detail: Some(
                "gateway session expired — re-link your Discord account (account menu, top right)"
                    .into(),
            ),
            me: None,
            sol_mode: None,
        });
    }
    if code == 401 || code == 403 {
        tracing::warn!(code, %body, "access probe: gateway revoked our credentials");
        return Ok(AccessCheck {
            state: "revoked",
            detail: Some(format!("gateway {code}: {body}")),
            me: None,
            sol_mode: None,
        });
    }
    Ok(AccessCheck {
        state: "unreachable",
        detail: Some(format!("gateway {code}: {body}")),
        me: None,
        sol_mode: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_base64url_sha256_of_verifier() {
        // Pin the PKCE shape against the frozen gateway contract:
        // challenge = base64url_nopad(sha256(verifier_string_bytes)).
        let verifier = "dGVzdC12ZXJpZmllci1zdHJpbmc";
        let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge.len(), 43); // 32 bytes → 43 chars, no padding
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
    }

    #[test]
    fn expiry_parses_rfc3339() {
        let mut a = DesktopAuth {
            token: "t".into(),
            expires_at: Some("2000-01-01T00:00:00Z".into()),
            discord: DiscordProfile {
                id: "1".into(),
                username: "u".into(),
                avatar: None,
            },
            gateway_base: "https://x".into(),
        };
        assert!(a.expired());
        a.expires_at = Some("2999-01-01T00:00:00Z".into());
        assert!(!a.expired());
        a.expires_at = Some("garbage".into());
        assert!(!a.expired()); // unparseable → assume valid, gateway decides
        a.expires_at = None;
        assert!(!a.expired());
    }

    #[test]
    fn auth_callback_url_shape_parses() {
        let url = tauri::Url::parse("degenbox://auth/callback?code=abc123").unwrap();
        assert_eq!(url.scheme(), "degenbox");
        assert_eq!(url.host_str(), Some("auth"));
        assert_eq!(url.path(), "/callback");
        let code = url
            .query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned());
        assert_eq!(code.as_deref(), Some("abc123"));
    }
}
