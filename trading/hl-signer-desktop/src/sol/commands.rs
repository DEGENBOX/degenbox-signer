//! Headless `sol …` subcommands.
//!
//! UX conventions follow the binary's HL side (branded output, refuse
//! to overwrite, password prompts hidden) and the keystore semantics
//! follow `signer-cli` / the Tauri app (same encrypted format, same
//! shared on-disk locations).

use crate::branding;
use crate::sol::config::SolConfig;
use crate::sol::runtime::{self, AuthSource, SolRuntimeInner, SpawnArgs};
use anyhow::{anyhow, Context, Result};
use degenbox_signer_core::{
    generate, import_extension_json, load_from_path, save_to_path, Keystore, Signer as _,
    SignerSlot,
};
use std::path::PathBuf;
use std::sync::Arc;

fn read_password(prompt: &str) -> Result<String> {
    let p = format!("  {} {prompt}", branding::prefix());
    rpassword::prompt_password(&p).context("read password")
}

fn read_password_stdin_or_prompt(stdin: bool, prompt: &str) -> Result<String> {
    if stdin {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        Ok(buf.trim_end_matches(['\r', '\n']).to_string())
    } else {
        read_password(prompt)
    }
}

/// `sol init` — generate a fresh keypair + encrypted keystore.
pub fn init(out: Option<PathBuf>) -> Result<()> {
    let path = match out {
        Some(p) => p,
        None => crate::sol::default_keystore_path()?,
    };
    if path.exists() {
        return Err(anyhow!(
            "{} already exists — refusing to overwrite. Move it aside first.",
            path.display()
        ));
    }
    println!("{}", branding::wordmark());
    let password = read_password("New password (≥ 8 chars): ")?;
    let confirm = read_password("Confirm password:          ")?;
    if password != confirm {
        return Err(anyhow!("passwords don't match"));
    }
    if password.len() < 8 {
        return Err(anyhow!("password must be at least 8 characters"));
    }
    println!(
        "  {} generating keypair + deriving Argon2id key (~200ms)…",
        branding::prefix()
    );
    let (ks, _kp) = generate(&password).context("generate keystore")?;
    save_to_path(&ks, &path).context("write keystore")?;
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::muted("Keystore written:"),
        branding::ink(&path.display().to_string())
    );
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::muted("Pubkey:          "),
        branding::accent_bold(&ks.pubkey)
    );
    println!();
    println!(
        "  Fund this address with SOL, then run {}",
        branding::accent("hl-signer-desktop sol daemon")
    );
    Ok(())
}

/// `sol import` — adopt an existing keystore file (validate + copy) or
/// re-encrypt a DegenBox Chrome-extension export into the native format.
pub fn import(
    file: Option<PathBuf>,
    extension_json: Option<PathBuf>,
    out: Option<PathBuf>,
) -> Result<()> {
    let target = match out {
        Some(p) => p,
        None => crate::sol::default_keystore_path()?,
    };
    if target.exists() {
        return Err(anyhow!(
            "{} already exists — refusing to overwrite. Move it aside first.",
            target.display()
        ));
    }
    println!("{}", branding::wordmark());
    match (file, extension_json) {
        (Some(src), None) => {
            // Same encrypted format (signer-core keystore) → validate
            // shape, then copy verbatim. No password needed; the source
            // file is untouched.
            let bytes = std::fs::read(&src).context("read source keystore")?;
            let ks: Keystore =
                serde_json::from_slice(&bytes).context("source is not a DegenBox keystore")?;
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, &bytes).context("write keystore")?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600));
            }
            println!(
                "  {} {} {}",
                branding::tick(),
                branding::muted("Keystore adopted:"),
                branding::ink(&target.display().to_string())
            );
            println!(
                "  {} {} {}",
                branding::tick(),
                branding::muted("Pubkey:          "),
                branding::accent_bold(&ks.pubkey)
            );
            println!(
                "  {} {}",
                branding::muted("·"),
                branding::muted("Same password as the source file — the envelope is unchanged.")
            );
            Ok(())
        }
        (None, Some(json_path)) => {
            let json = std::fs::read_to_string(&json_path).context("read extension export")?;
            let password = read_password("Extension keystore password: ")?;
            let (ks, kp) =
                import_extension_json(&json, &password).map_err(|e| anyhow!("import: {e}"))?;
            save_to_path(&ks, &target).context("write keystore")?;
            println!(
                "  {} {} {}",
                branding::tick(),
                branding::muted("Extension wallet imported:"),
                branding::ink(&target.display().to_string())
            );
            println!(
                "  {} {} {}",
                branding::tick(),
                branding::muted("Pubkey:                   "),
                branding::accent_bold(&kp.pubkey().to_string())
            );
            Ok(())
        }
        _ => Err(anyhow!(
            "pass exactly one of --file <keystore.json> or --extension-json <export.json>"
        )),
    }
}

/// `sol pubkey` — print the keystore's pubkey (no password needed; the
/// pubkey is stored unencrypted for UI convenience).
pub fn pubkey(keystore: Option<PathBuf>) -> Result<()> {
    let path = crate::sol::resolve_keystore_path(keystore)?;
    let pk = crate::sol::peek_pubkey(&path)
        .ok_or_else(|| anyhow!("{} is not a DegenBox keystore", path.display()))?;
    println!("{pk}");
    Ok(())
}

/// `sol budget` — persist the mandatory copy-session budget (and the
/// optional per-token cap) into the shared `sol-config.json`.
pub fn budget(session_sol: f64, per_token_sol: Option<f64>) -> Result<()> {
    if !session_sol.is_finite() || session_sol <= 0.0 {
        return Err(anyhow!("--session-sol must be > 0"));
    }
    let mut cfg = SolConfig::load_or_default();
    cfg.copy_session_sol = Some(session_sol);
    if per_token_sol.is_some() {
        cfg.copy_per_token_sol = per_token_sol.filter(|s| s.is_finite() && *s > 0.0);
    }
    cfg.save().map_err(|e| anyhow!("save sol-config: {e}"))?;
    println!("{}", branding::wordmark());
    println!(
        "  {} copy-session budget set: {} SOL{}",
        branding::tick(),
        branding::accent_bold(&format!("{session_sol}")),
        cfg.copy_per_token_sol
            .map(|p| format!(" (per-token cap {p} SOL)"))
            .unwrap_or_default()
    );
    println!(
        "  {} {}",
        branding::muted("·"),
        branding::muted("applies on the next `sol daemon` run / TUI unlock")
    );
    Ok(())
}

pub struct SolDaemonArgs {
    pub keystore: Option<PathBuf>,
    pub password_stdin: bool,
    pub gateway: String,
    pub rpc_url: Option<String>,
    pub port: u16,
    pub token_file: Option<PathBuf>,
    pub session_sol: Option<f64>,
    pub per_token_sol: Option<f64>,
    /// Skip the sell/copy execution streams — serve only the localhost
    /// signer-protocol daemon for the web app.
    pub serve_only: bool,
}

/// `sol daemon` — the headless Solana executor: unlocks the keystore,
/// serves the `127.0.0.1:5829` signer-protocol daemon (web-app
/// detection + quote/swap), and runs the sell+copy execution streams.
pub async fn daemon(args: SolDaemonArgs) -> Result<()> {
    let ks_path = crate::sol::resolve_keystore_path(args.keystore)?;
    eprintln!("{}", branding::wordmark());
    let password = read_password_stdin_or_prompt(args.password_stdin, "Keystore password: ")?;
    let kp = load_from_path(&ks_path, &password).map_err(|e| anyhow!("decrypt keystore: {e}"))?;
    let pubkey = kp.pubkey().to_string();
    eprintln!(
        "  {} {}  {} {}",
        branding::brand_tag(),
        branding::status_pill("ready"),
        branding::muted("sol wallet"),
        branding::accent_bold(&pubkey)
    );

    let sol_cfg = {
        let mut c = SolConfig::load_or_default();
        if let Some(s) = args.session_sol {
            c.copy_session_sol = Some(s);
        }
        if let Some(p) = args.per_token_sol {
            c.copy_per_token_sol = Some(p);
        }
        if let Some(r) = args.rpc_url.clone() {
            c.rpc_url = Some(r);
        }
        c
    };

    // 1. The :5829 signer-protocol daemon — lockable slot armed with the
    //    unlocked keypair; the web app probes /health to detect us and
    //    pushes its session token via /setAuth.
    let slot = SignerSlot::default();
    slot.install(insecure_clone(&kp)?);
    let daemon_state = degenbox_signer_core::LocalDaemonState::new(
        slot,
        args.gateway.clone(),
        sol_cfg.resolved_rpc_url(),
    )
    .with_client_kind("hl-signer-desktop");
    let web_config = daemon_state.config.clone();
    let port = args.port;
    let serve_task =
        tokio::spawn(
            async move { degenbox_signer_core::serve_local_daemon(daemon_state, port).await },
        );

    // 2. The execution runtime (sell + copy streams).
    let rt: Arc<SolRuntimeInner> = Arc::new(SolRuntimeInner::default());
    if !args.serve_only {
        let auth = match args.token_file {
            Some(tf) => {
                let token = std::fs::read_to_string(&tf)
                    .context("read token file")?
                    .trim()
                    .to_string();
                if token.is_empty() {
                    return Err(anyhow!("token file is empty"));
                }
                AuthSource::Token {
                    base: args.gateway.clone(),
                    token,
                }
            }
            None => AuthSource::Auto {
                web: Some(web_config),
            },
        };
        if sol_cfg.copy_session_sol.is_none() {
            eprintln!(
                "  {} copy buys DISARMED — no session budget. Set one with \
                 `sol budget --session-sol <SOL>` or pass --session-sol. \
                 TP/SL + mirror sells still execute.",
                branding::warn("!")
            );
        }
        runtime::spawn(
            rt.clone(),
            SpawnArgs {
                kp: Arc::new(kp),
                auth,
                cfg: sol_cfg,
                stdout_log: true,
            },
        );
    }

    // 3. Block on the HTTP daemon; a bind failure (port in use) is a
    //    clean, actionable error rather than a silent no-serve.
    match serve_task.await {
        Ok(r) => r.map_err(|e| anyhow!("{e}")),
        Err(e) => Err(anyhow!("local daemon task panicked: {e}")),
    }
}

/// `Keypair` doesn't implement `Clone`; round-trip through the secret
/// bytes (zeroized by the caller's drop) so the slot and the runtime can
/// each own one. Process-local only — the bytes never leave RAM.
fn insecure_clone(kp: &degenbox_signer_core::Keypair) -> Result<degenbox_signer_core::Keypair> {
    degenbox_signer_core::Keypair::try_from(&kp.to_bytes()[..])
        .map_err(|e| anyhow!("keypair clone: {e}"))
}
