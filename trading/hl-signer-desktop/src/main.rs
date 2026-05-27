//! `hl-signer-desktop` — DegenBox Hyperliquid local desktop signer.
//!
//! Mirrors the legacy Go bot
//! (`legay-hyperliquid-bot/degenbox-client/`) at the wire level: an
//! encrypted API agent key lives on the user's machine, this binary
//! holds the only copy, and trades the DegenBox server queues for the
//! user get signed locally and POSTed to HL — the server never sees
//! the private key.
//!
//! Subcommands:
//!
//! - `setup`     — interactive wizard: prompt for HL API agent
//!                  private key + passphrase → write encrypted
//!                  keystore + initial config.
//! - `register`  — one-time hello to the DegenBox server so its
//!                  `/signer/status` endpoint flips to "ready" for the
//!                  matching user.
//! - `daemon`    — long-running loop: poll the server for queued
//!                  instructions, sign locally, POST to HL, report
//!                  back. NATS push for sub-second nudges (optional).
//! - `migrate`   — adopt a legacy Go-bot keystore as the v2 keystore
//!                  (same envelope; no re-encrypt needed).

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

mod config;
mod daemon;
mod hl_info;
mod keystore;
mod self_update;
mod server;
mod signing;
mod tui;

#[derive(Parser, Debug)]
#[command(
    name = "hl-signer-desktop",
    version,
    about = "DegenBox Hyperliquid local desktop signer (self-custody)"
)]
struct Cli {
    /// Force-skip the interactive TUI even when no subcommand is
    /// given. Useful for systemd / Docker entrypoints that wrap the
    /// daemon — they want the bare CLI behavior.
    #[arg(long, global = true)]
    headless: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Interactive setup wizard — generate an encrypted keystore from
    /// your HL API agent private key.
    Setup {
        #[arg(long)]
        keystore: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Skip the interactive prompts (suitable for scripts). The
        /// private key is read from stdin; everything else uses
        /// defaults.
        #[arg(long)]
        non_interactive: bool,
    },
    /// One-time register with the DegenBox server. Reads the agent
    /// address out of the keystore (no passphrase needed).
    Register {
        #[arg(long)]
        keystore: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Server URL override — also persisted into the config file.
        #[arg(long)]
        server: Option<String>,
        /// API token issued by the DegenBox dashboard
        /// (Account → API tokens). Persisted into the config file
        /// once accepted.
        #[arg(long)]
        token: Option<String>,
        /// Your HL master account (the 0x… wallet the agent acts on
        /// behalf of). REQUIRED for Close / TP / SL instructions —
        /// the signer must query HL `/info` against this address to
        /// resolve the live position size and direction.
        #[arg(long)]
        account: Option<String>,
    },
    /// Daemon mode — long-running loop: poll (and optionally
    /// NATS-subscribe) for queued HL instructions, sign with the
    /// local keystore, POST to HL, report back.
    Daemon {
        #[arg(long)]
        keystore: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Read the keystore passphrase from stdin instead of
        /// prompting interactively. Useful for headless setups.
        #[arg(long)]
        password_stdin: bool,
        /// Poll cadence (seconds).
        #[arg(long, default_value_t = 3)]
        poll_secs: u64,
        /// Optional NATS URL — when set, the daemon also subscribes
        /// to push nudges to react sub-second.
        #[arg(long)]
        nats_url: Option<String>,
    },
    /// Adopt a legacy Go-bot keystore as the v2 keystore. The legacy
    /// file uses the SAME envelope (Argon2id t=3, m=64MB, p=4 +
    /// AES-256-GCM) so this is a `cp` + a passphrase check, no
    /// re-encryption.
    Migrate {
        /// Path to the legacy `degenbox.keystore.json`.
        #[arg(long)]
        from: PathBuf,
        /// Destination — defaults to the v2 path
        /// (`~/.config/degenbox/hl-keystore.json`).
        #[arg(long)]
        to: Option<PathBuf>,
    },
    /// Print the agent address stored in the keystore. No
    /// passphrase required.
    Address {
        #[arg(long)]
        keystore: Option<PathBuf>,
    },
    /// Check the latest GitHub release for a newer build. Prints the
    /// new tag if one is available; exits cleanly when up to date.
    /// Use `self-update` to actually install.
    CheckUpdate,
    /// Download the latest release for the current platform, verify
    /// its sha256 against the release's SHASUMS256.txt, and atomically
    /// replace the running binary. Restart the daemon to pick up the
    /// new version.
    SelfUpdate,
    /// Open the interactive TUI explicitly. Same behavior as running
    /// `hl-signer-desktop` with no subcommand at all.
    Tui,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    // No-subcommand + TTY + not --headless → TUI. This is the path
    // power users hit; systemd / Docker entrypoints pass --headless
    // (or one of the explicit subcommands) and get the old behavior.
    let want_tui = match &cli.cmd {
        None => !cli.headless && is_tty(),
        Some(Cmd::Tui) => true,
        _ => false,
    };
    if want_tui {
        // The TUI installs its own tracing layer (so the Logs tab can
        // tail the buffer); don't initialize the fmt subscriber first
        // or it'll race for the global default.
        return tui::run().await;
    }

    init_tracing();
    let Some(cmd) = cli.cmd else {
        // Non-TTY / --headless with no subcommand — print the help
        // text so cron / `nohup` users get something actionable
        // instead of an opaque "expected a subcommand" panic.
        use clap::CommandFactory;
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };
    match cmd {
        Cmd::Setup {
            keystore,
            config,
            non_interactive,
        } => cmd_setup(keystore, config, non_interactive),
        Cmd::Register {
            keystore,
            config,
            server,
            token,
            account,
        } => cmd_register(keystore, config, server, token, account).await,
        Cmd::Daemon {
            keystore,
            config,
            password_stdin,
            poll_secs,
            nats_url,
        } => cmd_daemon(keystore, config, password_stdin, poll_secs, nats_url).await,
        Cmd::Migrate { from, to } => cmd_migrate(from, to),
        Cmd::Address { keystore } => cmd_address(keystore),
        Cmd::CheckUpdate => cmd_check_update().await,
        Cmd::SelfUpdate => self_update::run_self_update().await,
        Cmd::Tui => tui::run().await,
    }
}

/// Best-effort TTY check. When stdin/stdout isn't a terminal (e.g.
/// systemd, Docker -d, `| less`) the user clearly does not want a
/// full-screen TUI — fall back to printing help.
fn is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

async fn cmd_check_update() -> Result<()> {
    match self_update::check().await {
        Some(info) => {
            println!(
                "  update available: {} → {} ({})",
                info.current, info.latest, info.tag
            );
            println!("  release notes:    {}", info.html_url);
            println!("  install with:     hl-signer-desktop self-update");
        }
        None => {
            println!("  up to date ({}).", env!("CARGO_PKG_VERSION"));
        }
    }
    Ok(())
}

fn cmd_setup(
    keystore: Option<PathBuf>,
    config_path: Option<PathBuf>,
    non_interactive: bool,
) -> Result<()> {
    let ks_path = keystore
        .map(Ok)
        .unwrap_or_else(config::default_keystore_path)?;
    let cfg_path = config_path
        .map(Ok)
        .unwrap_or_else(config::default_config_path)?;

    println!();
    println!("  DegenBox HL Signer — Setup");
    println!("  -------------------------------");
    println!();
    println!("  1. Visit https://app.hyperliquid.xyz/API");
    println!("  2. Generate an API wallet (an \"API agent\") for your account.");
    println!("  3. Copy the private key — paste it below.");
    println!();

    let private_key = if non_interactive {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("read private key from stdin")?;
        line.trim().to_string()
    } else {
        rpassword::prompt_password("  Private key (hex, hidden): ")?
            .trim()
            .to_string()
    };
    if private_key.is_empty() {
        return Err(anyhow!("private key is required"));
    }

    let passphrase = if non_interactive {
        // In scripted mode use a fixed passphrase passed via env.
        std::env::var("HL_SIGNER_PASSPHRASE")
            .map_err(|_| anyhow!("HL_SIGNER_PASSPHRASE env required in --non-interactive"))?
    } else {
        let pw1 = rpassword::prompt_password("  Encryption passphrase: ")?;
        let pw2 = rpassword::prompt_password("  Repeat passphrase:     ")?;
        if pw1 != pw2 {
            return Err(anyhow!("passphrases did not match"));
        }
        pw1
    };

    let address = keystore::encrypt_and_save(&private_key, passphrase.as_bytes(), &ks_path)?;

    let mut cfg = config::load(&cfg_path).unwrap_or_default();
    cfg.agent_address = Some(address.clone());
    config::save(&cfg_path, &cfg)?;

    println!();
    println!("  Keystore written: {}", ks_path.display());
    println!("  Agent address:    {}", address);
    println!("  Config:           {}", cfg_path.display());
    println!();
    println!("  Next:");
    println!("    hl-signer-desktop register --server=<url> --token=<api_token>");
    println!("    hl-signer-desktop daemon");
    println!();
    Ok(())
}

async fn cmd_register(
    keystore_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    server_override: Option<String>,
    token_override: Option<String>,
    account_override: Option<String>,
) -> Result<()> {
    let ks_path = keystore_path
        .map(Ok)
        .unwrap_or_else(config::default_keystore_path)?;
    let cfg_path = config_path
        .map(Ok)
        .unwrap_or_else(config::default_config_path)?;

    let agent_address = keystore::peek_address(&ks_path)
        .context("read agent address from keystore (run `setup` first)")?;
    let mut cfg = config::load(&cfg_path).unwrap_or_default();
    if let Some(s) = server_override {
        cfg.server_url = s;
    }
    if let Some(a) = account_override {
        let a = a.trim().to_ascii_lowercase();
        if !(a.starts_with("0x") && a.len() == 42 && a[2..].chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Err(anyhow!(
                "--account must be a 0x-prefixed 20-byte hex address (got {a})"
            ));
        }
        if a.eq_ignore_ascii_case(&agent_address) {
            return Err(anyhow!(
                "--account must be your HL master wallet, NOT the agent address \
                 ({a}). The agent is sandboxed and has no positions; positions \
                 are owned by the master."
            ));
        }
        cfg.account_address = Some(a);
    }

    let raw_token = token_override
        .clone()
        .or_else(|| cfg.api_token.clone())
        .ok_or_else(|| anyhow!("--token is required on first register"))?;

    // One-shot onboarding tokens are 32 hex chars (16 random bytes,
    // hex-encoded). Anything else is treated as a long-lived API token
    // and uses the legacy bearer-auth register path.
    let is_registration_token =
        raw_token.len() == 32 && raw_token.chars().all(|c| c.is_ascii_hexdigit());

    let resp = if is_registration_token {
        let req = server::RedeemRegistrationReq {
            token: raw_token,
            agent_address: agent_address.clone(),
            client_version: Some(format!("hl-signer-desktop {}", env!("CARGO_PKG_VERSION"))),
            host_id: cfg.host_id.clone(),
        };
        server::ServerClient::redeem_registration(&cfg.server_url, &req).await?
    } else {
        cfg.api_token = Some(raw_token.clone());
        let client = server::ServerClient::new(cfg.server_url.clone(), raw_token)?;
        client
            .register(&server::RegisterReq {
                agent_address: agent_address.clone(),
                client_version: Some(format!("hl-signer-desktop {}", env!("CARGO_PKG_VERSION"))),
                host_id: cfg.host_id.clone(),
            })
            .await?
    };
    cfg.agent_address = Some(agent_address.clone());
    config::save(&cfg_path, &cfg)?;
    println!("  Registered.");
    println!("  User:    {}", resp.user_id);
    println!("  Agent:   {}", resp.agent_address);
    println!("  Server:  {}", cfg.server_url);
    match &cfg.account_address {
        Some(a) => println!("  Account: {a}"),
        None => println!(
            "  Account: (not set — Close / TP / SL will fail until you run \
             `register --account=0x…`)"
        ),
    }
    Ok(())
}

async fn cmd_daemon(
    keystore_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    password_stdin: bool,
    poll_secs: u64,
    nats_url: Option<String>,
) -> Result<()> {
    let ks_path = keystore_path
        .map(Ok)
        .unwrap_or_else(config::default_keystore_path)?;
    let cfg_path = config_path
        .map(Ok)
        .unwrap_or_else(config::default_config_path)?;
    let cfg = config::load(&cfg_path).context("load config — run `setup` first")?;
    let pass = read_passphrase(password_stdin)?;
    let (secret_hex, address) = keystore::decrypt(&ks_path, pass.as_bytes())?;
    let opts = daemon::DaemonOpts {
        config: cfg,
        secret_hex,
        agent_address: address,
        poll_interval: Duration::from_secs(poll_secs.max(1)),
        nats_url,
        user_id: None,
    };
    // v1 parity: probe for a newer release on startup, then re-check
    // once every 24h while the daemon is alive. The check is
    // best-effort and silent on failure — see `self_update::check`.
    if let Some(info) = self_update::check().await {
        eprintln!();
        eprintln!(
            "  update available: {} → {} ({}). Run `hl-signer-desktop self-update` to upgrade.",
            info.current, info.latest, info.tag
        );
        eprintln!();
    }
    self_update::spawn_daily_check();
    daemon::run(opts).await
}

fn cmd_migrate(from: PathBuf, to: Option<PathBuf>) -> Result<()> {
    let to_path = to.map(Ok).unwrap_or_else(config::default_keystore_path)?;
    if to_path.exists() {
        return Err(anyhow!(
            "destination keystore already exists at {} — refusing to overwrite",
            to_path.display()
        ));
    }
    let parent = to_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    std::fs::copy(&from, &to_path).context("copy legacy keystore to v2 location")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&to_path, std::fs::Permissions::from_mode(0o600))?;
    }
    let address = keystore::peek_address(&to_path)?;
    println!("  Legacy keystore adopted at {}", to_path.display());
    println!("  Agent address: {}", address);
    println!("  Same passphrase as the legacy bot — the envelope is unchanged.");
    Ok(())
}

fn cmd_address(keystore_path: Option<PathBuf>) -> Result<()> {
    let ks_path = keystore_path
        .map(Ok)
        .unwrap_or_else(config::default_keystore_path)?;
    let addr = keystore::peek_address(&ks_path)?;
    println!("{addr}");
    Ok(())
}

fn read_passphrase(stdin: bool) -> Result<String> {
    if stdin {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        Ok(buf.trim_end_matches(['\r', '\n']).to_string())
    } else {
        Ok(rpassword::prompt_password("  Keystore passphrase: ")?)
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hl_signer_desktop=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

// Local dotenv stand-in — we only use it best-effort to pick up
// `HL_SIGNER_PASSPHRASE` and similar from a `.env` next to the binary.
mod dotenvy {
    pub fn dotenv() -> Result<(), ()> {
        let path = std::path::Path::new(".env");
        let Ok(s) = std::fs::read_to_string(path) else {
            return Err(());
        };
        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                if std::env::var(k.trim()).is_err() {
                    std::env::set_var(k.trim(), v.trim().trim_matches('"'));
                }
            }
        }
        Ok(())
    }
}
