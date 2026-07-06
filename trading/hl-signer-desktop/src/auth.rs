//! Discord desktop-auth for headless boxes — the CLI equivalent of the
//! Tauri app's PKCE browser hand-off (`signer-app/src-tauri/src/auth.rs`),
//! against the SAME gateway contract and the SAME on-disk credential
//! file, so app + CLI on one machine share one linked account:
//!
//! 1. `login` generates a random 32-byte `verifier` (base64url) and
//!    prints `{gateway}/api/auth/discord/start?flow=desktop&challenge=
//!    <base64url(sha256(verifier))>` for the user to open in ANY
//!    browser (their laptop is fine — the one-time code is bound to the
//!    verifier, not to the machine or IP).
//! 2. After Discord authorizes, the gateway 302s the browser to
//!    `degenbox://auth/callback?code=<one-time>`. On a box without the
//!    desktop app the deep link goes nowhere — the user copies the code
//!    (or the whole `degenbox://…` URL) out of the address bar and
//!    pastes it at our prompt. See the honest UX note in `login`.
//! 3. We `POST {gateway}/api/auth/desktop/exchange {"code","verifier"}`
//!    and persist the minted JWT to `~/.config/degenbox/
//!    desktop-auth.json` (0600, atomic) — byte-compatible with the app.
//!
//! The token then feeds gateway auth exactly like in the app: the
//! Solana runtime's `resolve_auth` reads it first (before the HL
//! pairing JWT and the web-app `/setAuth` push).

use crate::branding;
use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use rand_core::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write as _;
use std::path::{Path, PathBuf};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

// ─── persisted account (same file as the desktop app) ───────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordProfile {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub avatar: Option<String>,
}

/// On-disk shape of `desktop-auth.json` — kept field-for-field
/// identical to `signer-app`'s `DesktopAuth` so the app and the CLI
/// read/write ONE file.
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

/// `~/.config/degenbox/desktop-auth.json` — the exact path the Tauri
/// app uses (`signer-core::default_dir()`).
pub fn auth_path() -> Result<PathBuf, String> {
    Ok(degenbox_signer_core::default_dir()
        .map_err(|e| e.to_string())?
        .join("desktop-auth.json"))
}

impl DesktopAuth {
    pub fn load() -> Option<Self> {
        Self::load_from(&auth_path().ok()?)
    }

    pub fn load_from(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// True when `expires_at` is present, parseable and in the past.
    /// Unparseable / absent expiries are treated as still-valid — the
    /// gateway rejects a genuinely dead token with a clearer error than
    /// a silent client-side drop (same policy as the app).
    pub fn expired(&self) -> bool {
        self.expires_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|t| t < chrono::Utc::now())
    }

    pub fn load_valid() -> Option<Self> {
        Self::load().filter(|a| !a.expired())
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(&auth_path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
        tmp.write_all(&bytes).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600));
        }
        tmp.persist(path).map_err(|e| e.error.to_string())?;
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

// ─── callback input parsing ─────────────────────────────────────────

/// Map the gateway's stable failure short-codes to operator-readable
/// copy (same set the app maps: see W6 contract).
fn error_copy(short: &str) -> String {
    match short {
        "access_denied" => "login cancelled at the Discord consent screen".into(),
        "oauth_failed" => "Discord round-trip failed on the gateway — try again".into(),
        "server_error" => "gateway-side error during login — try again".into(),
        "invalid_state" => "stale login state on the gateway — start over".into(),
        other => format!("login failed ({other})"),
    }
}

/// Accept what the user pastes after authorizing:
/// - the full deep link `degenbox://auth/callback?code=<code>`,
/// - any URL carrying a `code=` query param,
/// - or the bare code itself.
///
/// `?error=<short>` deep links become a readable failure.
pub fn parse_callback_input(input: &str) -> Result<String, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("nothing pasted — copy the code (or the degenbox:// URL) and try again".into());
    }
    // URL-ish input: pull the query params out, tolerating any scheme.
    if let Some(q) = s.split_once('?').map(|(_, q)| q) {
        let mut code = None;
        let mut error = None;
        for pair in q.split('&') {
            match pair.split_once('=') {
                Some(("code", v)) if !v.is_empty() => code = Some(v.to_string()),
                Some(("error", v)) if !v.is_empty() => error = Some(v.to_string()),
                _ => {}
            }
        }
        if let Some(short) = error {
            return Err(error_copy(&short));
        }
        return code.ok_or_else(|| "the pasted URL carries no code= parameter".into());
    }
    // Bare code: base64url charset, sane length (gateway mints 32 random
    // bytes → 43 chars; accept a generous range so a format tweak
    // doesn't brick the CLI).
    if s.len() >= 16
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Ok(s.to_string());
    }
    Err("that doesn't look like a one-time code or a degenbox:// callback URL".into())
}

// ─── exchange wire shapes (frozen W6 contract) ──────────────────────

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

fn resolve_base(server: Option<String>) -> String {
    server
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            crate::config::Config::load_or_default()
                .server_url
                .trim_end_matches('/')
                .to_string()
        })
}

/// `login` — headless Discord link. Prints the start URL, waits for the
/// pasted one-time code, exchanges it for a gateway JWT and persists it
/// to the shared `desktop-auth.json`.
pub async fn login(server: Option<String>) -> Result<()> {
    let base = resolve_base(server);

    let mut raw = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut raw);
    let verifier = B64.encode(raw);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    let url = format!("{base}/api/auth/discord/start?flow=desktop&challenge={challenge}");

    println!("{}", branding::wordmark());
    println!("{}", branding::heading("Connect your Discord account"));
    println!();
    println!(
        "  {} Open this URL in a browser — {} works (phone / laptop):",
        branding::prefix(),
        branding::accent_bold("any device")
    );
    println!();
    println!("    {}", branding::accent(&url));
    println!();
    println!(
        "  {} After you authorize, the browser is redirected to a",
        branding::prefix()
    );
    println!(
        "     {} link. Without the desktop app installed that link",
        branding::accent_bold("degenbox://auth/callback?code=…")
    );
    println!("     opens nothing — that is expected. Copy the URL from the");
    println!(
        "     address bar (or just the {} value) and paste it below.",
        branding::accent_bold("code=")
    );
    println!(
        "     {}",
        branding::muted(
            "Tip: Firefox shows the degenbox:// URL in the address bar; Chrome may \
             only show an \"open app?\" dialog — use Firefox if you can't see the code."
        )
    );
    println!();
    print!("  {} Paste the code (or full URL): ", branding::prefix());
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("read code from stdin")?;
    let code = parse_callback_input(&line).map_err(|e| anyhow!(e))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("build http client")?;
    let resp = client
        .post(format!("{base}/api/auth/desktop/exchange"))
        .json(&ExchangeReq {
            code: &code,
            verifier: &verifier,
        })
        .send()
        .await
        .with_context(|| format!("POST {base}/api/auth/desktop/exchange"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 404 {
            return Err(anyhow!(
                "the gateway at {base} does not support desktop login yet (404) — \
                 pair with a connect token instead (`hl-signer-desktop register`)"
            ));
        }
        return Err(anyhow!(
            "exchange rejected ({status}): {body} — one-time codes expire after \
             ~2 minutes and are single-use; run `login` again"
        ));
    }
    let parsed: ExchangeResp = resp.json().await.context("decode exchange response")?;

    let auth = DesktopAuth {
        token: parsed.token,
        expires_at: parsed.expires_at,
        discord: parsed.discord,
        gateway_base: base.clone(),
    };
    auth.save().map_err(|e| anyhow!("save desktop-auth: {e}"))?;

    println!();
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::muted("Linked as:"),
        branding::accent_bold(&auth.discord.username)
    );
    if let Some(exp) = &auth.expires_at {
        println!(
            "  {} {} {}",
            branding::muted("·"),
            branding::muted("Token expires:"),
            branding::ink(exp)
        );
    }
    println!(
        "  {} {} {}",
        branding::muted("·"),
        branding::muted("Stored at:    "),
        branding::ink(&auth_path().map_err(|e| anyhow!(e))?.display().to_string())
    );
    println!(
        "  {} {}",
        branding::muted("·"),
        branding::muted(
            "The Solana runtime + clients commands use this token automatically \
             (shared with the desktop app)."
        )
    );
    Ok(())
}

/// `logout` — remove the persisted Discord credential.
pub fn logout() -> Result<()> {
    let was_linked = DesktopAuth::load().is_some();
    DesktopAuth::clear().map_err(|e| anyhow!(e))?;
    println!("{}", branding::wordmark());
    if was_linked {
        println!(
            "  {} {}",
            branding::tick(),
            branding::muted("Discord account unlinked (desktop-auth.json removed).")
        );
    } else {
        println!(
            "  {} {}",
            branding::muted("·"),
            branding::muted("No linked Discord account.")
        );
    }
    Ok(())
}

/// `account` — show the linked Discord account, if any.
pub fn account() -> Result<()> {
    println!("{}", branding::wordmark());
    match DesktopAuth::load() {
        Some(a) => {
            println!(
                "  {} {} {}",
                branding::tick(),
                branding::muted("Linked:  "),
                branding::accent_bold(&a.discord.username)
            );
            println!(
                "  {} {} {}",
                branding::muted("·"),
                branding::muted("Discord: "),
                branding::ink(&a.discord.id)
            );
            println!(
                "  {} {} {}",
                branding::muted("·"),
                branding::muted("Gateway: "),
                branding::ink(&a.gateway_base)
            );
            match (&a.expires_at, a.expired()) {
                (Some(exp), true) => println!(
                    "  {} {} {}",
                    branding::warn("!"),
                    branding::muted("Expired: "),
                    branding::warn(&format!("{exp} — run `hl-signer-desktop login` again"))
                ),
                (Some(exp), false) => println!(
                    "  {} {} {}",
                    branding::muted("·"),
                    branding::muted("Expires: "),
                    branding::ink(exp)
                ),
                (None, _) => {}
            }
        }
        None => {
            println!(
                "  {} {}",
                branding::muted("·"),
                branding::muted("No Discord account linked. Run: hl-signer-desktop login")
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_is_base64url_sha256_of_verifier() {
        // Pin the PKCE shape against the frozen gateway contract (same
        // pin the app carries).
        let verifier = "dGVzdC12ZXJpZmllci1zdHJpbmc";
        let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
        assert_eq!(challenge.len(), 43);
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
    }

    #[test]
    fn parses_bare_code_and_full_deeplink() {
        let code = "Ab1-_Cd2EfGh3456IjKl7890MnOpQrStUvWxYz12345";
        assert_eq!(parse_callback_input(code).unwrap(), code);
        assert_eq!(
            parse_callback_input(&format!("degenbox://auth/callback?code={code}")).unwrap(),
            code
        );
        // Trailing params + whitespace tolerated.
        assert_eq!(
            parse_callback_input(&format!("  degenbox://auth/callback?code={code}&x=1 \n"))
                .unwrap(),
            code
        );
        // Any scheme works — the user may paste a wrapped URL.
        assert_eq!(
            parse_callback_input(&format!("https://whatever/cb?code={code}")).unwrap(),
            code
        );
    }

    #[test]
    fn error_deeplinks_and_garbage_are_rejected_readably() {
        let e = parse_callback_input("degenbox://auth/callback?error=access_denied").unwrap_err();
        assert!(e.contains("cancelled"), "{e}");
        assert!(parse_callback_input("").is_err());
        assert!(parse_callback_input("not a code!!!").is_err());
        assert!(parse_callback_input("degenbox://auth/callback?code=").is_err());
        // Too short to be a one-time code.
        assert!(parse_callback_input("abc").is_err());
    }

    #[test]
    fn expiry_parses_rfc3339_like_the_app() {
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
    fn save_load_roundtrip_is_app_shape_compatible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop-auth.json");
        let a = DesktopAuth {
            token: "jwt-token".into(),
            expires_at: Some("2031-01-01T00:00:00Z".into()),
            discord: DiscordProfile {
                id: "42".into(),
                username: "henri".into(),
                avatar: Some("https://cdn.discordapp.com/avatars/42/x.png?size=128".into()),
            },
            gateway_base: "https://api-v2.degenbox.app".into(),
        };
        a.save_to(&path).unwrap();
        // The persisted JSON must carry exactly the app's field names.
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for key in ["token", "expires_at", "discord", "gateway_base"] {
            assert!(raw.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(raw["discord"]["username"], "henri");
        let b = DesktopAuth::load_from(&path).unwrap();
        assert_eq!(b.token, a.token);
        assert_eq!(b.discord.id, "42");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "desktop-auth.json must be 0600");
        }
    }
}
