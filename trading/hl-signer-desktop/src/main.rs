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

// The module-header doc comments use hanging-indented continuation lines
// for the subcommand / module-layout lists (text aligned under the item
// label, not the marker). Clippy's `doc_overindented_list_items` flags
// that purely-cosmetic alignment; we keep the readable alignment.
#![allow(clippy::doc_overindented_list_items)]

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::EnvFilter;

mod audit;
mod auth;
mod branding;
mod clients;
mod config;
mod daemon;
mod hl_info;
mod keystore;
mod panic;
mod self_update;
mod server;
mod sol;
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
        /// Force overwrite if the keystore already exists. Suppresses
        /// the overwrite prompt.
        #[arg(long)]
        force: bool,
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
    ///
    /// MULTI-BOT / MULTI-WALLET:
    /// To run multiple bots for different wallets, run each instance with a custom
    /// config directory, e.g.:
    ///   hl-signer-desktop daemon --config ~/.degenbox-bot2/config.json --keystore ~/.degenbox-bot2/keystore.json
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
    /// EMERGENCY kill-switch — cancel ALL resting orders and close ALL
    /// open positions for the configured HL master account, signed
    /// locally and POSTed straight to HL. Works offline (no server).
    Panic {
        #[arg(long)]
        keystore: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Read the keystore passphrase from stdin instead of prompting.
        #[arg(long)]
        password_stdin: bool,
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
    /// Solana side of the unified signer — keystore management, the
    /// 127.0.0.1:5829 web bridge and the sell/copy execution streams.
    #[command(subcommand)]
    Sol(SolCmd),
    /// Multi-wallet vault (shared with the DegenBox Signer desktop
    /// app): N Solana + N Hyperliquid wallets under ONE master
    /// password in `~/.config/degenbox/vault/`.
    #[command(subcommand)]
    Clients(ClientsCmd),
    /// Connect your Discord account (headless flow): prints the auth
    /// URL to open in ANY browser, then accepts the pasted one-time
    /// code. The minted gateway token is shared with the desktop app.
    Login {
        /// Gateway base URL override (else the configured server).
        #[arg(long)]
        server: Option<String>,
    },
    /// Remove the linked Discord account (deletes desktop-auth.json).
    Logout,
    /// Show the linked Discord account, if any.
    Account,
    /// Multi-client runtime — the headless equivalent of the unlocked
    /// desktop app: unlocks ALL vault wallets with the master password;
    /// the HL primary runs the full poll/sign/report daemon, HL
    /// standbys keep heartbeat+balance alive, the Sol primary runs the
    /// sell/copy engine + the :5829 web bridge.
    Run {
        /// Read the master password from stdin (else
        /// $DEGENBOX_MASTER_PASSWORD, else a hidden prompt).
        #[arg(long)]
        password_stdin: bool,
        /// HL poll cadence override (seconds; else the pairing config).
        #[arg(long)]
        poll_secs: Option<u64>,
        /// Optional NATS push URL for the HL primary daemon.
        #[arg(long)]
        nats_url: Option<String>,
        /// Port for the :5829 signer-protocol web bridge.
        #[arg(long, default_value_t = 5829)]
        port: u16,
        /// Path to a JWT file for gateway auth (else Discord login /
        /// HL pairing JWT / web-app push).
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Copy-session budget override (SOL) for copy buys.
        #[arg(long)]
        session_sol: Option<f64>,
    },
}

#[derive(Subcommand, Debug)]
enum ClientsCmd {
    /// List all clients: local vault wallets merged with the gateway
    /// registry (`GET /api/trading/clients`). Works fully offline.
    List {
        /// Machine-readable JSON instead of the table.
        #[arg(long)]
        json: bool,
        /// Skip the gateway merge (local vault only).
        #[arg(long)]
        no_gateway: bool,
    },
    /// Generate a fresh wallet into the vault. Creates the vault on
    /// first use (HL agent keys are minted on hyperliquid.xyz — use
    /// `import` for those).
    Add {
        /// "sol" (the only locally generatable chain).
        #[arg(long, default_value = "sol")]
        chain: String,
        #[arg(long)]
        label: Option<String>,
        /// Read the master password from stdin instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Import a pasted private key into the vault (sol: base58/hex
    /// seed or 64-byte keypair; hl: 32-byte hex agent key).
    Import {
        /// "sol" | "hl".
        #[arg(long)]
        chain: String,
        #[arg(long)]
        label: Option<String>,
        /// Read the private key from the FIRST stdin line (scripts).
        #[arg(long)]
        secret_stdin: bool,
        /// Read the master password from stdin (after the secret when
        /// --secret-stdin is also set).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Remove a wallet from the vault. Non-destructive: the encrypted
    /// keystore is kept as `<file>.removed.bak`.
    Remove {
        /// Client id, unique id prefix, address, or label.
        selector: String,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Set (or clear) a client's label.
    Label {
        selector: String,
        /// New label; omit to clear.
        label: Option<String>,
    },
    /// Pause a client (per-client kill-switch, persisted in the vault
    /// + pushed to the gateway best-effort).
    Pause { selector: String },
    /// Resume a paused client.
    Resume { selector: String },
    /// Designate a client as its chain's primary executor.
    SetPrimary { selector: String },
}

#[derive(Subcommand, Debug)]
enum SolCmd {
    /// Generate a fresh Solana keypair + encrypted keystore
    /// (`~/.config/degenbox/sol-keystore.json`, shared with the
    /// DegenBox Signer desktop app).
    Init {
        /// Where to write the keystore JSON.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Adopt an existing wallet: either another DegenBox keystore file
    /// (signer-cli / desktop app — validate + copy, same password) or a
    /// Chrome-extension export (decrypt + re-encrypt natively).
    Import {
        /// Path to an existing DegenBox keystore JSON.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Path to a DegenBox Chrome-extension keystore export (JSON).
        #[arg(long)]
        extension_json: Option<PathBuf>,
        /// Destination — defaults to the shared keystore path.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Print the keystore's pubkey (no password required).
    Pubkey {
        #[arg(long)]
        keystore: Option<PathBuf>,
    },
    /// Persist the mandatory copy-session budget into `sol-config.json`
    /// (shared with the desktop app). Copy BUYS are refused while no
    /// budget is set; TP/SL + mirror sells always run.
    Budget {
        /// Hard client-side SOL spend ceiling for copy buys per session.
        #[arg(long)]
        session_sol: f64,
        /// Optional per-token cap (SOL) on top of the session cap.
        #[arg(long)]
        per_token_sol: Option<f64>,
    },
    /// Headless Solana executor: unlock the keystore, serve the
    /// 127.0.0.1:5829 signer-protocol daemon (web-app detection +
    /// quote/swap), and run the TP/SL-sell + copy-trade execution
    /// streams. Gateway auth: --token-file, else the HL pairing JWT in
    /// hl-config.json, else the session token the web app pushes to the
    /// local daemon.
    Daemon {
        #[arg(long)]
        keystore: Option<PathBuf>,
        /// Read the keystore password from stdin instead of prompting.
        #[arg(long)]
        password_stdin: bool,
        /// Gateway base URL (initial value — the web app may override
        /// via /setGateway).
        #[arg(long, default_value = "https://api-v2.degenbox.app")]
        gateway: String,
        /// Solana RPC override (else sol-config.json / SOLANA_RPC_URL /
        /// public mainnet-beta).
        #[arg(long)]
        rpc_url: Option<String>,
        /// Port to bind on 127.0.0.1. Default matches what the web app
        /// probes; change only for testing.
        #[arg(long, default_value_t = 5829)]
        port: u16,
        /// Path to a JWT file for gateway auth (optional — see above).
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Session budget override (SOL) for copy buys — else the value
        /// persisted via `sol budget` is used.
        #[arg(long)]
        session_sol: Option<f64>,
        /// Per-token cap override (SOL).
        #[arg(long)]
        per_token_sol: Option<f64>,
        /// Serve only the :5829 web bridge; skip the execution streams.
        #[arg(long)]
        serve_only: bool,
    },
}

/// Make classic Windows conhost / PowerShell render our output correctly:
/// switch the output codepage to UTF-8 (so `›`, `✓`, `≥`, `—` aren't mojibake)
/// and enable virtual-terminal processing (so ANSI color escapes are
/// interpreted instead of printed as `←[38;2;…m`). No-op off Windows.
#[cfg(windows)]
fn init_windows_console() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, SetConsoleOutputCP,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_OUTPUT_HANDLE,
    };
    const CP_UTF8: u32 = 65001;
    unsafe {
        SetConsoleOutputCP(CP_UTF8);
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if !h.is_null() {
            let mut mode: u32 = 0;
            if GetConsoleMode(h, &mut mode) != 0 {
                SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

#[cfg(not(windows))]
fn init_windows_console() {}

#[tokio::main]
async fn main() -> Result<()> {
    init_windows_console();
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
        println!("{}", branding::wordmark());
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
            force,
        } => cmd_setup(keystore, config, non_interactive, force).await,
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
        Cmd::Panic {
            keystore,
            config,
            password_stdin,
        } => panic::run_panic(keystore, config, password_stdin).await,
        Cmd::CheckUpdate => cmd_check_update().await,
        Cmd::SelfUpdate => self_update::run_self_update().await,
        Cmd::Tui => tui::run().await,
        Cmd::Sol(sol_cmd) => match sol_cmd {
            SolCmd::Init { out } => sol::commands::init(out),
            SolCmd::Import {
                file,
                extension_json,
                out,
            } => sol::commands::import(file, extension_json, out),
            SolCmd::Pubkey { keystore } => sol::commands::pubkey(keystore),
            SolCmd::Budget {
                session_sol,
                per_token_sol,
            } => sol::commands::budget(session_sol, per_token_sol),
            SolCmd::Daemon {
                keystore,
                password_stdin,
                gateway,
                rpc_url,
                port,
                token_file,
                session_sol,
                per_token_sol,
                serve_only,
            } => {
                sol::commands::daemon(sol::commands::SolDaemonArgs {
                    keystore,
                    password_stdin,
                    gateway,
                    rpc_url,
                    port,
                    token_file,
                    session_sol,
                    per_token_sol,
                    serve_only,
                })
                .await
            }
        },
        Cmd::Clients(cmd) => match cmd {
            ClientsCmd::List { json, no_gateway } => clients::cmd_list(json, no_gateway).await,
            ClientsCmd::Add {
                chain,
                label,
                password_stdin,
            } => clients::cmd_add(chain, label, password_stdin),
            ClientsCmd::Import {
                chain,
                label,
                secret_stdin,
                password_stdin,
            } => clients::cmd_import(chain, label, secret_stdin, password_stdin),
            ClientsCmd::Remove { selector, yes } => clients::cmd_remove(selector, yes),
            ClientsCmd::Label { selector, label } => clients::cmd_label(selector, label),
            ClientsCmd::Pause { selector } => clients::cmd_pause(selector, true).await,
            ClientsCmd::Resume { selector } => clients::cmd_pause(selector, false).await,
            ClientsCmd::SetPrimary { selector } => clients::cmd_set_primary(selector),
        },
        Cmd::Login { server } => auth::login(server).await,
        Cmd::Logout => auth::logout(),
        Cmd::Account => auth::account(),
        Cmd::Run {
            password_stdin,
            poll_secs,
            nats_url,
            port,
            token_file,
            session_sol,
        } => {
            clients::cmd_run(clients::RunArgs {
                password_stdin,
                poll_secs,
                nats_url,
                port,
                token_file,
                session_sol,
            })
            .await
        }
    }
}

/// Resolve the HL keystore for the legacy single-wallet commands
/// (`daemon` / `register` / `address`): explicit flag > the legacy
/// global file > the vault primary's keystore (read-through after the
/// app/CLI migrated the legacy file into the shared vault — same
/// envelope, same passphrase).
fn resolve_hl_keystore_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let legacy = config::default_keystore_path()?;
    if legacy.exists() {
        return Ok(legacy);
    }
    if let Some(p) = clients::vault_primary_hl_keystore() {
        tracing::info!(path = %p.display(), "using the vault primary HL keystore");
        return Ok(p);
    }
    Ok(legacy)
}

/// Best-effort TTY check. When stdin/stdout isn't a terminal (e.g.
/// systemd, Docker -d, `| less`) the user clearly does not want a
/// full-screen TUI — fall back to printing help.
fn is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stdin().is_terminal()
}

async fn cmd_check_update() -> Result<()> {
    println!("{}", branding::wordmark());
    match self_update::check().await {
        Some(info) => {
            println!(
                "  {} update available: {} {} {} ({})",
                branding::prefix(),
                branding::accent_bold(&info.current),
                branding::muted("→"),
                branding::accent_bold(&info.latest),
                branding::muted(&info.tag),
            );
            println!(
                "  {} release notes:    {}",
                branding::muted("·"),
                branding::ink(&info.html_url)
            );
            println!(
                "  {} install with:     {}",
                branding::muted("·"),
                branding::accent("hl-signer-desktop self-update")
            );
        }
        None => {
            println!(
                "  {} up to date ({}).",
                branding::tick(),
                branding::accent_bold(env!("CARGO_PKG_VERSION"))
            );
        }
    }
    Ok(())
}

async fn cmd_setup(
    keystore: Option<PathBuf>,
    config_path: Option<PathBuf>,
    non_interactive: bool,
    force: bool,
) -> Result<()> {
    let ks_path = keystore
        .map(Ok)
        .unwrap_or_else(config::default_keystore_path)?;
    let cfg_path = config_path
        .map(Ok)
        .unwrap_or_else(config::default_config_path)?;

    println!("{}", branding::wordmark());
    println!(
        "{}",
        branding::heading("Setup — connect your Hyperliquid API agent")
    );
    println!();
    println!(
        "  The signer needs an {} — a sandboxed key that can {} trade,",
        branding::accent_bold("API AGENT key"),
        branding::accent_bold("ONLY")
    );
    println!(
        "  never withdraw. It is {} your main wallet.",
        branding::accent_bold("NOT")
    );
    println!();
    println!(
        "  {} paste your MetaMask / main-wallet private key here.",
        branding::warn("⚠ NEVER")
    );
    println!(
        "     That key controls your {}. An agent key can only place trades.",
        branding::accent_bold("funds")
    );
    println!();
    println!("  {}", branding::ink("How to get the agent key:"));
    println!(
        "    {} Visit {} {}",
        branding::prefix(),
        branding::accent("https://app.hyperliquid.xyz/API"),
        branding::muted("(connected with your main wallet)")
    );
    println!(
        "    {} Click {} {}",
        branding::prefix(),
        branding::accent_bold("\"Generate\""),
        branding::muted("— HL creates a NEW agent with its OWN address + key")
    );
    println!(
        "    {} Copy {} key (not your MetaMask key) and paste it below",
        branding::prefix(),
        branding::accent_bold("THAT")
    );
    println!();

    let private_key = if non_interactive {
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .context("read private key from stdin")?;
        line.trim().to_string()
    } else {
        let prompt = format!("  {} Agent Private Key (hex, hidden): ", branding::prefix());
        rpassword::prompt_password(&prompt)?.trim().to_string()
    };
    if private_key.is_empty() {
        return Err(anyhow!("private key is required"));
    }
    // Normalise — strip optional 0x prefix, then validate exactly 64 hex chars.
    let private_key = private_key.trim_start_matches("0x").to_string();
    if private_key.len() != 64 || !private_key.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "private key must be a 32-byte hex string (64 hex chars, optionally prefixed with 0x); got {} chars",
            private_key.len()
        ));
    }

    // SAFETY: derive the address this key controls and let the user verify it's
    // a sandboxed AGENT, not their main/MetaMask wallet. The #1 onboarding
    // mistake — and a real fund-loss risk — is pasting the main-wallet key. We
    // also ask HL whether the address holds funds: an API agent is ALWAYS empty
    // (positions + balance live on the master), so a positive balance is a
    // near-certain sign the wrong key was pasted. Best-effort: if HL is
    // unreachable we fall back to a manual confirm, never block on it.
    if !non_interactive {
        if let Some(addr) = key_address(&private_key) {
            println!();
            println!(
                "  {} This key controls the address: {}",
                branding::prefix(),
                branding::accent_bold(&addr)
            );
            use std::io::Write;
            match account_value_usd(&addr).await {
                Some(v) if v > 0.0 => {
                    println!();
                    println!(
                        "  {} {} holds ${:.2} on Hyperliquid.",
                        branding::warn("⚠ STOP:"),
                        branding::accent_bold(&addr),
                        v
                    );
                    println!("     API agents are ALWAYS empty — funds live on your main wallet —");
                    println!(
                        "     so this looks like your {} key, NOT an agent. You almost",
                        branding::accent_bold("MAIN wallet")
                    );
                    println!("     certainly pasted the wrong key.");
                    println!(
                        "     {} Generate an API agent at app.hyperliquid.xyz/API and use THAT key.",
                        branding::prefix()
                    );
                    print!(
                        "  {} To override anyway (NOT recommended) type exactly {}: ",
                        branding::prefix(),
                        branding::accent_bold("i-know-this-is-my-agent")
                    );
                    std::io::stdout().flush()?;
                    let mut a = String::new();
                    std::io::stdin().read_line(&mut a)?;
                    if a.trim() != "i-know-this-is-my-agent" {
                        println!("  Aborted — paste your sandboxed API-agent key instead.");
                        return Ok(());
                    }
                }
                _ => {
                    println!(
                        "     {}",
                        branding::muted("(empty on HL — consistent with a sandboxed agent)")
                    );
                    print!(
                        "  {} Confirm this is the generated Agent Key (and NOT your main wallet)? [y/N]: ",
                        branding::prefix()
                    );
                    std::io::stdout().flush()?;
                    let mut a = String::new();
                    std::io::stdin().read_line(&mut a)?;
                    if !matches!(a.trim().to_lowercase().as_str(), "y" | "yes") {
                        println!("  Aborted — generate an API agent and paste THAT key.");
                        return Ok(());
                    }
                }
            }
        }
    }

    // Warn if a keystore already exists — overwrite only after confirmation.
    if ks_path.exists() && !force {
        if non_interactive {
            return Err(anyhow!(
                "keystore already exists — use --force to overwrite"
            ));
        }
        println!(
            "\n  {} Keystore already exists at {}",
            branding::warn("!"),
            branding::ink(&ks_path.display().to_string())
        );
        print!("  {} Overwrite? [y/N]: ", branding::prefix());
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("  Aborted.");
            return Ok(());
        }
    }

    let passphrase = if non_interactive {
        // In scripted mode use a fixed passphrase passed via env.
        std::env::var("HL_SIGNER_PASSPHRASE")
            .map_err(|_| anyhow!("HL_SIGNER_PASSPHRASE env required in --non-interactive"))?
    } else {
        let p1 = format!(
            "  {} Encryption passphrase (≥ 8 chars): ",
            branding::prefix()
        );
        let p2 = format!(
            "  {} Repeat passphrase:                  ",
            branding::prefix()
        );
        loop {
            let pw1 = rpassword::prompt_password(&p1)?;
            if pw1.len() < 8 {
                println!("  Passphrase must be at least 8 characters — try again.");
                continue;
            }
            let pw2 = rpassword::prompt_password(&p2)?;
            if pw1 != pw2 {
                println!("  Passphrases did not match — try again.");
                continue;
            }
            break pw1;
        }
    };

    let address = keystore::encrypt_and_save(&private_key, passphrase.as_bytes(), &ks_path)?;

    let mut cfg = config::load(&cfg_path).unwrap_or_default();
    cfg.agent_address = Some(address.clone());
    config::save(&cfg_path, &cfg)?;

    println!();
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::muted("Keystore written:"),
        branding::ink(&ks_path.display().to_string())
    );
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::muted("Agent address:   "),
        branding::accent_bold(&address)
    );
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::muted("Config:          "),
        branding::ink(&cfg_path.display().to_string())
    );
    println!();
    println!("  {}", branding::accent_bold("Next:"));
    println!(
        "    {}",
        branding::accent("hl-signer-desktop register --server=<url> --token=<api_token>")
    );
    println!("    {}", branding::accent("hl-signer-desktop"));
    println!();
    Ok(())
}

/// Derive the 0x address a private-key hex string controls, WITHOUT writing a
/// keystore — used to show the user which address they're about to import so
/// they can catch a wrong (main-wallet) key. `None` on a malformed key.
fn key_address(private_key_hex: &str) -> Option<String> {
    let bytes = hex::decode(private_key_hex).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    keystore::derive_address(&arr).ok()
}

/// Best-effort HL `marginSummary.accountValue` (USD) for an address. An API
/// agent is ALWAYS 0 (funds + positions live on the master), so a positive
/// value flags a main-wallet key pasted by mistake. Returns `None` on any
/// network / parse failure — setup then falls back to the manual confirm and
/// never blocks on HL being reachable.
async fn account_value_usd(address: &str) -> Option<f64> {
    let resp = reqwest::Client::new()
        .post("https://api.hyperliquid.xyz/info")
        .json(&serde_json::json!({"type": "clearinghouseState", "user": address}))
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?
        .json::<serde_json::Value>()
        .await
        .ok()?;
    resp.get("marginSummary")?
        .get("accountValue")?
        .as_str()?
        .parse::<f64>()
        .ok()
}

/// Prompt (visible) for the 6-digit TOTP code on stdin. Used by `register`
/// when the server answers 428 `totp_required`. The code rotates every 30s so
/// it is not secret — a hidden prompt isn't needed.
fn prompt_totp_code() -> Result<String> {
    use std::io::Write;
    eprintln!();
    eprintln!("  2FA required — enter the 6-digit code from your authenticator app.");
    eprint!("  > Code: ");
    std::io::stderr().flush().ok();
    let mut s = String::new();
    std::io::stdin()
        .read_line(&mut s)
        .context("read TOTP code from stdin")?;
    let code = s.trim().to_string();
    if code.is_empty() {
        return Err(anyhow!("no 2FA code entered"));
    }
    Ok(code)
}

async fn cmd_register(
    keystore_path: Option<PathBuf>,
    config_path: Option<PathBuf>,
    server_override: Option<String>,
    token_override: Option<String>,
    account_override: Option<String>,
) -> Result<()> {
    let ks_path = resolve_hl_keystore_path(keystore_path)?;
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
        // The HL master wallet is REQUIRED to pair: the gateway only hands
        // out instructions when `paired_with_account` is set. Registering
        // without it produces a signer that shows "Ready" but silently
        // never receives an order. Fail loud here instead.
        let paired_with_account = cfg.account_address.clone().ok_or_else(|| {
            anyhow!(
                "your HL master wallet is required to pair — re-run with \
                 `--account 0x…` (your Hyperliquid main account, NOT the agent \
                 address). Without it the signer registers but never receives \
                 any trades."
            )
        })?;
        let mut req = server::RedeemRegistrationReq {
            token: raw_token,
            agent_address: agent_address.clone(),
            client_version: Some(format!("hl-signer-desktop {}", env!("CARGO_PKG_VERSION"))),
            host_id: cfg.host_id.clone(),
            paired_with_account: Some(paired_with_account),
            totp_code: None,
        };
        // First try without a 2FA code. If the account has 2FA enrolled the
        // server answers 428 `totp_required` — prompt for the 6-digit code and
        // retry once with it inline.
        match server::ServerClient::redeem_registration(&cfg.server_url, &req).await {
            Ok(r) => r,
            Err(server::ServerError::Status(status, body))
                if status.as_u16() == 428 && body.contains("totp_required") =>
            {
                let code = prompt_totp_code()?;
                req.totp_code = Some(code);
                server::ServerClient::redeem_registration(&cfg.server_url, &req).await?
            }
            Err(e) => return Err(e.into()),
        }
    } else {
        cfg.api_token = Some(raw_token.clone());
        let client = server::ServerClient::new(cfg.server_url.clone(), raw_token)?;
        client
            .register(&server::RegisterReq {
                agent_address: agent_address.clone(),
                client_version: Some(format!("hl-signer-desktop {}", env!("CARGO_PKG_VERSION"))),
                host_id: cfg.host_id.clone(),
                // Declare the pairing on the bearer path too — without it
                // the row lands approved-but-unpaired and never receives
                // a trade (the gateway preserves an existing pairing when
                // None, so re-registering can't wipe it).
                paired_with_account: cfg.account_address.clone(),
            })
            .await?
    };
    // Persist the signer JWT the redeem flow minted so the daemon can
    // authenticate its `/signer/*` polling. (The legacy bearer branch already
    // set `api_token` from the supplied token and its response carries none.)
    if let Some(tok) = &resp.api_token {
        cfg.api_token = Some(tok.clone());
    }
    cfg.agent_address = Some(agent_address.clone());
    config::save(&cfg_path, &cfg)?;
    println!("{}", branding::wordmark());
    println!(
        "  {} {}",
        branding::tick(),
        branding::accent_bold("Registered.")
    );
    let user_display = resp.discord_handle.as_deref().unwrap_or(&resp.user_id);
    println!(
        "  {} {} {}",
        branding::muted("·"),
        branding::muted("User:   "),
        branding::ink(user_display)
    );
    println!(
        "  {} {} {}",
        branding::muted("·"),
        branding::muted("Agent:  "),
        branding::accent_bold(&resp.agent_address)
    );
    println!(
        "  {} {} {}",
        branding::muted("·"),
        branding::muted("Server: "),
        branding::ink(&cfg.server_url)
    );
    match &cfg.account_address {
        Some(a) => println!(
            "  {} {} {}",
            branding::muted("·"),
            branding::muted("Account:"),
            branding::accent_bold(a)
        ),
        None => println!(
            "  {} {} {}",
            branding::warn("!"),
            branding::muted("Account:"),
            branding::warn(
                "(not set — Close / TP / SL will fail until you run `register --account=0x…`)"
            )
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
    let ks_path = resolve_hl_keystore_path(keystore_path)?;
    let cfg_path = config_path
        .map(Ok)
        .unwrap_or_else(config::default_config_path)?;
    let cfg = config::load(&cfg_path).context("load config — run `setup` first")?;
    let pass = read_passphrase(password_stdin)?;
    let (secret_hex, address) = keystore::decrypt(&ks_path, pass.as_bytes())?;
    // CLI `daemon` honours the per-bot configured cadence unless the
    // operator passed an explicit non-default --poll-secs (3 is the flag
    // default), so a headless run matches what the TUI would do.
    let effective_poll = if poll_secs == 3 {
        cfg.poll_secs
    } else {
        poll_secs
    };
    let opts = daemon::DaemonOpts {
        config: cfg,
        secret_hex,
        agent_address: address,
        poll_interval: Duration::from_secs(effective_poll.max(1)),
        nats_url,
        pause: None,
        runtime: None,
        hl_runtime: daemon::fresh_hl_runtime(),
        paper_mode: std::env::var("HL_SIGNER_PAPER")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false),
        // Single-bot CLI: keep the marker ledger alongside this bot's config
        // file (its own dir), not a hard-coded global path.
        config_dir: cfg_path.parent().map(|p| p.to_path_buf()),
        executed_path: None,
        // Legacy single-wallet daemon: claim everything for this user
        // (exact pre-vault behaviour).
        claim_scope: daemon::ClaimScope::Unscoped,
        banner: true,
        stdin_totp: true,
    };
    // Branded daemon banner. Tracing is initialised by `init_tracing`
    // upstream; we print this once before the daemon's poll loop so
    // operators see a clear marker in journald / `script` recordings.
    eprintln!("{}", branding::wordmark());
    eprintln!(
        "  {} {}  {}",
        branding::brand_tag(),
        branding::status_pill("connecting"),
        branding::muted("registering with server…")
    );
    // v1 parity: probe for a newer release on startup, then re-check
    // once every 24h while the daemon is alive. The check is
    // best-effort and silent on failure — see `self_update::check`.
    if let Some(info) = self_update::check().await {
        eprintln!();
        eprintln!(
            "  {} {} {} {} {} ({}). Run {} to upgrade.",
            branding::brand_tag(),
            branding::warn("\u{2191} update available:"),
            branding::accent_bold(&info.current),
            branding::muted("→"),
            branding::accent_bold(&info.latest),
            branding::muted(&info.tag),
            branding::accent("`hl-signer-desktop self-update`")
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
    println!("{}", branding::wordmark());
    println!(
        "  {} {} {}",
        branding::tick(),
        branding::muted("Legacy keystore adopted at"),
        branding::ink(&to_path.display().to_string())
    );
    println!(
        "  {} {} {}",
        branding::muted("·"),
        branding::muted("Agent address:"),
        branding::accent_bold(&address)
    );
    println!(
        "  {} {}",
        branding::muted("·"),
        branding::muted("Same passphrase as the legacy bot — the envelope is unchanged.")
    );
    Ok(())
}

fn cmd_address(keystore_path: Option<PathBuf>) -> Result<()> {
    let ks_path = resolve_hl_keystore_path(keystore_path)?;
    let addr = keystore::peek_address(&ks_path)?;
    println!("{addr}");
    Ok(())
}

pub(crate) fn read_passphrase(stdin: bool) -> Result<String> {
    if stdin {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        Ok(buf.trim_end_matches(['\r', '\n']).to_string())
    } else {
        let prompt = format!("  {} Keystore passphrase: ", branding::prefix());
        Ok(rpassword::prompt_password(&prompt)?)
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
