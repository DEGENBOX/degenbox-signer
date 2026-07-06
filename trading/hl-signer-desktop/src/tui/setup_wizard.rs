//! First-run setup wizard — 4 steps.
//!
//! 1. Welcome — explain self-custody, key never leaves machine.
//! 2. Provide key — paste an existing private key (32 bytes hex) or
//!    generate a fresh one.
//! 3. Keystore backend — pick file (only option today; the OS-keychain
//!    backend will land in a follow-up sprint).
//! 4. Register — paste a one-shot registration token, POST it to the
//!    server, show a progress indicator, then a check mark.
//!
//! This runs INSIDE the same terminal handle as the main TUI so the
//! transition is seamless.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind};
use rand_core::{OsRng, RngCore};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::config::{self};
use crate::keystore;
use crate::server;

use super::events::{EventStream, TuiEvent};
use super::theme::Theme;
use super::widgets;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Welcome,
    KeySource,
    Backend,
    Account,
    Register,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Generate,
    Import,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    File,
    Keyring,
}

pub enum WizardOutcome {
    Completed,
    Quit,
}

#[derive(Debug)]
struct State {
    step: Step,
    theme: Theme,
    selected_source: KeySource,
    selected_backend: Backend,
    private_key_hex: String,
    passphrase: String,
    confirm_passphrase: String,
    pass_focus: bool, // false = passphrase field, true = confirm field
    /// HL master account (`0x…`) this signer pairs to. Collected in the
    /// Account step; REQUIRED for in-TUI registration (the gateway only
    /// delivers instructions to a paired signer).
    account_address: String,
    /// Client name shown in the multi-client dashboard.
    client_name: String,
    /// Account step focus: false = account field, true = name field.
    account_focus_name: bool,
    register_token: String,
    register_status: RegisterStatus,
    error: Option<String>,
    server_url: String,
    ks_path: PathBuf,
    cfg_path: PathBuf,
    /// `true` when the user has reached the "Generated" state and
    /// should be shown the secret + a back-up reminder.
    show_generated: bool,
}

#[derive(Debug, Clone)]
enum RegisterStatus {
    Idle,
    InFlight,
    Ok { user_id: String, agent: String },
    Failed(String),
}

pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ks_path: PathBuf,
    cfg_path: PathBuf,
) -> Result<WizardOutcome> {
    let cfg = config::load(&cfg_path).unwrap_or_default();
    let mut state = State {
        step: Step::Welcome,
        theme: Theme::from_env(),
        selected_source: KeySource::Generate,
        selected_backend: Backend::File,
        private_key_hex: String::new(),
        passphrase: String::new(),
        confirm_passphrase: String::new(),
        pass_focus: false,
        account_address: cfg.account_address.clone().unwrap_or_default(),
        client_name: cfg.client_name.clone().unwrap_or_else(|| {
            // Default the client name to the bot directory name so the hub
            // dashboard isn't full of "client_2" rows.
            cfg_path
                .parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "default".into())
        }),
        account_focus_name: false,
        register_token: String::new(),
        register_status: RegisterStatus::Idle,
        error: None,
        server_url: cfg.server_url.clone(),
        ks_path,
        cfg_path,
        show_generated: false,
    };

    let mut events = EventStream::new(Duration::from_millis(250));

    loop {
        terminal.draw(|f| render(f, &state))?;
        match events.next().await {
            Some(TuiEvent::Term(Event::Key(k))) if k.kind != KeyEventKind::Release => {
                let action = handle_key(&mut state, k.code).await;
                match action {
                    WizardAction::Quit => return Ok(WizardOutcome::Quit),
                    WizardAction::Done => return Ok(WizardOutcome::Completed),
                    WizardAction::Continue => {}
                    WizardAction::Register => {
                        // Paint the in-flight spinner BEFORE the blocking
                        // network call so the UI shows progress, not a freeze.
                        terminal.draw(|f| render(f, &state))?;
                        match attempt_register(&mut state).await {
                            Ok(()) => state.step = Step::Done,
                            Err(e) => {
                                state.register_status = RegisterStatus::Failed(e.to_string());
                            }
                        }
                    }
                }
            }
            Some(TuiEvent::Tick) => {}
            Some(_) => {}
            None => return Ok(WizardOutcome::Quit),
        }
    }
}

enum WizardAction {
    Continue,
    Quit,
    Done,
    /// The user submitted the registration token. The caller draws an
    /// in-flight frame (so the spinner paints) BEFORE awaiting the
    /// network round-trip, which would otherwise freeze the UI 2-3s.
    Register,
}

async fn handle_key(state: &mut State, code: KeyCode) -> WizardAction {
    state.error = None;
    match (state.step, code) {
        (_, KeyCode::Esc) => WizardAction::Quit,
        (Step::Welcome, KeyCode::Enter) => {
            state.step = Step::KeySource;
            WizardAction::Continue
        }
        (Step::KeySource, KeyCode::Left | KeyCode::Right | KeyCode::Tab) => {
            state.selected_source = match state.selected_source {
                KeySource::Generate => KeySource::Import,
                KeySource::Import => KeySource::Generate,
            };
            state.show_generated = false;
            WizardAction::Continue
        }
        (Step::KeySource, KeyCode::Char(c)) if state.selected_source == KeySource::Import => {
            if c.is_ascii_hexdigit() || c == 'x' || c == 'X' {
                state.private_key_hex.push(c);
            }
            WizardAction::Continue
        }
        (Step::KeySource, KeyCode::Backspace) if state.selected_source == KeySource::Import => {
            state.private_key_hex.pop();
            WizardAction::Continue
        }
        (Step::KeySource, KeyCode::Char('g')) if state.selected_source == KeySource::Generate => {
            generate_into(state);
            WizardAction::Continue
        }
        (Step::KeySource, KeyCode::Enter) => {
            // Auto-generate when on Generate and not already done.
            if state.selected_source == KeySource::Generate && !state.show_generated {
                generate_into(state);
                return WizardAction::Continue;
            }
            if state.private_key_hex.trim().is_empty() {
                state.error = Some("Private key required.".into());
                return WizardAction::Continue;
            }
            state.step = Step::Backend;
            WizardAction::Continue
        }
        (Step::Backend, KeyCode::Left | KeyCode::Right | KeyCode::Tab) => {
            state.selected_backend = match state.selected_backend {
                Backend::File => Backend::Keyring,
                Backend::Keyring => Backend::File,
            };
            WizardAction::Continue
        }
        (Step::Backend, KeyCode::Char(c)) => {
            if state.pass_focus {
                state.confirm_passphrase.push(c);
            } else {
                state.passphrase.push(c);
            }
            WizardAction::Continue
        }
        (Step::Backend, KeyCode::Backspace) => {
            if state.pass_focus {
                state.confirm_passphrase.pop();
            } else {
                state.passphrase.pop();
            }
            WizardAction::Continue
        }
        (Step::Backend, KeyCode::Down | KeyCode::Up) => {
            state.pass_focus = !state.pass_focus;
            WizardAction::Continue
        }
        (Step::Backend, KeyCode::Enter) => {
            if state.selected_backend == Backend::Keyring {
                state.error = Some("OS keychain backend not implemented yet — using file.".into());
                state.selected_backend = Backend::File;
                return WizardAction::Continue;
            }
            if state.passphrase.len() < 8 {
                state.error = Some("Passphrase must be ≥ 8 chars.".into());
                return WizardAction::Continue;
            }
            if state.passphrase != state.confirm_passphrase {
                state.error = Some("Passphrases do not match.".into());
                return WizardAction::Continue;
            }
            match keystore::encrypt_and_save(
                &state.private_key_hex,
                state.passphrase.as_bytes(),
                &state.ks_path,
            ) {
                Ok(addr) => {
                    let mut cfg = config::load(&state.cfg_path).unwrap_or_default();
                    cfg.agent_address = Some(addr);
                    let _ = config::save(&state.cfg_path, &cfg);
                    state.step = Step::Account;
                }
                Err(e) => state.error = Some(e.to_string()),
            }
            WizardAction::Continue
        }
        // ─── Account step: collect the HL master wallet + client name ───
        (Step::Account, KeyCode::Up | KeyCode::Down | KeyCode::Tab) => {
            state.account_focus_name = !state.account_focus_name;
            WizardAction::Continue
        }
        (Step::Account, KeyCode::Char(c)) => {
            if state.account_focus_name {
                state.client_name.push(c);
            } else if c.is_ascii_hexdigit() || c == 'x' || c == 'X' {
                // Account field only accepts hex + the 0x marker.
                state.account_address.push(c);
            }
            WizardAction::Continue
        }
        (Step::Account, KeyCode::Backspace) => {
            if state.account_focus_name {
                state.client_name.pop();
            } else {
                state.account_address.pop();
            }
            WizardAction::Continue
        }
        (Step::Account, KeyCode::Enter) => {
            let agent = keystore::peek_address(&state.ks_path).ok();
            match validate_account(&state.account_address, agent.as_deref()) {
                Ok(normalized) => {
                    state.account_address = normalized.clone();
                    let mut cfg = config::load(&state.cfg_path).unwrap_or_default();
                    cfg.account_address = Some(normalized);
                    let name = state.client_name.trim();
                    if !name.is_empty() {
                        cfg.client_name = Some(name.to_string());
                    }
                    if let Err(e) = config::save(&state.cfg_path, &cfg) {
                        state.error = Some(format!("save config: {e}"));
                    } else {
                        state.step = Step::Register;
                    }
                }
                Err(e) => state.error = Some(e),
            }
            WizardAction::Continue
        }
        (Step::Register, KeyCode::Char('s')) => {
            // Skip — register later via the Wallet tab / CLI subcommand.
            state.step = Step::Done;
            WizardAction::Continue
        }
        (Step::Register, KeyCode::Char(c)) => {
            if c.is_ascii_hexdigit() {
                state.register_token.push(c);
            }
            WizardAction::Continue
        }
        (Step::Register, KeyCode::Backspace) => {
            state.register_token.pop();
            WizardAction::Continue
        }
        (Step::Register, KeyCode::Enter) => {
            // Flip to in-flight here; the caller paints the spinner frame
            // and then drives the actual network call so the UI doesn't
            // freeze during the round-trip.
            if state.register_token.trim().is_empty() {
                state.error = Some("registration token required.".into());
                return WizardAction::Continue;
            }
            state.register_status = RegisterStatus::InFlight;
            WizardAction::Register
        }
        (Step::Done, KeyCode::Enter) => WizardAction::Done,
        _ => WizardAction::Continue,
    }
}

/// Validate + normalize an HL master-account address. Must be a
/// 0x-prefixed 20-byte (40 hex char) EVM address, and MUST NOT equal the
/// agent address (the agent is sandboxed + holds no positions). Returns
/// the lowercased canonical form.
fn validate_account(raw: &str, agent: Option<&str>) -> Result<String, String> {
    let a = raw.trim().to_ascii_lowercase();
    if a.is_empty() {
        return Err("HL master wallet required — paste your 0x… account address.".into());
    }
    if !a.starts_with("0x") || a.len() != 42 || !a[2..].chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "must be a 0x-prefixed 20-byte hex address (40 hex chars); got {} chars",
            a.len()
        ));
    }
    if let Some(agent) = agent {
        if a.eq_ignore_ascii_case(agent) {
            return Err(
                "that's the AGENT address — paste your HL MASTER wallet (the one that holds funds)."
                    .into(),
            );
        }
    }
    Ok(a)
}

fn generate_into(state: &mut State) {
    let mut buf = [0u8; 32];
    OsRng.fill_bytes(&mut buf);
    state.private_key_hex = hex::encode(buf);
    state.show_generated = true;
}

async fn attempt_register(state: &mut State) -> Result<()> {
    let cfg = config::load(&state.cfg_path).unwrap_or_default();
    let agent_address =
        keystore::peek_address(&state.ks_path).map_err(|e| anyhow!("read agent address: {e}"))?;
    let token = state.register_token.trim().to_string();
    if token.is_empty() {
        return Err(anyhow!("registration token required"));
    }
    state.register_status = RegisterStatus::InFlight;

    let is_one_shot = token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit());
    let resp = if is_one_shot {
        // The HL master wallet is required to pair (the gateway only delivers
        // instructions when `paired_with_account` is set). The TUI wizard
        // doesn't collect it yet, so steer to the CLI `register` command
        // (which prompts/accepts `--account`) instead of registering a
        // signer that shows "Ready" but silently never trades.
        let paired_with_account = cfg.account_address.clone().ok_or_else(|| {
            anyhow!(
                "your HL master wallet is required to pair — finish with the CLI: \
                 `hl-signer-desktop register --token {token} --account 0x…` (your HL main \
                 account, not the agent). Without it no trades are ever delivered."
            )
        })?;
        let req = server::RedeemRegistrationReq {
            token,
            agent_address: agent_address.clone(),
            client_version: Some(format!("hl-signer-desktop {}", env!("CARGO_PKG_VERSION"))),
            host_id: cfg.host_id.clone(),
            paired_with_account: Some(paired_with_account),
            // The TUI onboarding path doesn't drive an interactive TOTP prompt;
            // 2FA-enrolled users are steered to the CLI `register` command,
            // which handles the 428 challenge. None keeps the wire shape valid.
            totp_code: None,
        };
        server::ServerClient::redeem_registration(&state.server_url, &req)
            .await
            .map_err(|e| anyhow!("{e}"))?
    } else {
        let client = server::ServerClient::new(state.server_url.clone(), token.clone())
            .map_err(|e| anyhow!("{e}"))?;
        client
            .register(&server::RegisterReq {
                agent_address: agent_address.clone(),
                client_version: Some(format!("hl-signer-desktop {}", env!("CARGO_PKG_VERSION"))),
                host_id: cfg.host_id.clone(),
                // Declare the pairing when the config already carries the
                // master account — None never wipes an existing pairing.
                paired_with_account: cfg.account_address.clone(),
            })
            .await
            .map_err(|e| anyhow!("{e}"))?
    };
    let mut cfg = config::load(&state.cfg_path).unwrap_or_default();
    cfg.agent_address = Some(resp.agent_address.clone());
    if !is_one_shot {
        cfg.api_token = Some(state.register_token.trim().to_string());
    }
    config::save(&state.cfg_path, &cfg).map_err(|e| anyhow!("{e}"))?;
    state.register_status = RegisterStatus::Ok {
        user_id: resp.user_id,
        agent: resp.agent_address,
    };
    Ok(())
}

fn render(frame: &mut Frame<'_>, state: &State) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let title = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "DegenBox HL Signer — First-run setup",
            state.theme.accent().add_modifier(Modifier::BOLD),
        ),
    ]);
    let p =
        Paragraph::new(vec![Line::from(""), title]).block(widgets::panel("Welcome", state.theme));
    frame.render_widget(p, chunks[0]);

    render_progress(frame, chunks[1], state);
    render_body(frame, chunks[2], state);
    render_footer(frame, chunks[3], state);
}

fn render_progress(frame: &mut Frame<'_>, area: Rect, state: &State) {
    let steps = [
        ("1 Welcome", Step::Welcome),
        ("2 Key", Step::KeySource),
        ("3 Backend", Step::Backend),
        ("4 Account", Step::Account),
        ("5 Register", Step::Register),
    ];
    let current_idx = match state.step {
        Step::Welcome => 0,
        Step::KeySource => 1,
        Step::Backend => 2,
        Step::Account => 3,
        Step::Register => 4,
        Step::Done => 5,
    };
    let mut spans = vec![Span::raw(" ")];
    for (i, (label, _)) in steps.iter().enumerate() {
        let style = if i == current_idx {
            state.theme.accent().add_modifier(Modifier::BOLD)
        } else if i < current_idx {
            state.theme.ok()
        } else {
            state.theme.muted()
        };
        spans.push(Span::styled(format!(" {label} "), style));
        if i + 1 < steps.len() {
            spans.push(Span::styled(" \u{203A} ", state.theme.muted()));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_body(frame: &mut Frame<'_>, area: Rect, state: &State) {
    let body = match state.step {
        Step::Welcome => render_welcome(state),
        Step::KeySource => render_key_source(state),
        Step::Backend => render_backend(state),
        Step::Account => render_account(state),
        Step::Register => render_register(state),
        Step::Done => render_done(state),
    };
    let title = match state.step {
        Step::Welcome => "About",
        Step::KeySource => "Provide a key",
        Step::Backend => "Encrypt the keystore",
        Step::Account => "Link your Hyperliquid account",
        Step::Register => "Register with the DegenBox server",
        Step::Done => "Ready",
    };
    let mut body = body;
    if let Some(err) = &state.error {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            format!("  {err}"),
            state.theme.err(),
        )));
    }
    let p = Paragraph::new(body)
        .wrap(Wrap { trim: false })
        .block(widgets::panel(title, state.theme));
    frame.render_widget(p, area);
}

fn render_welcome(state: &State) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("")];
    for l in crate::tui::logo::LOGO {
        lines.push(Line::from(Span::styled(
            *l,
            state.theme.accent().add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(Span::styled(
        format!("    {}", crate::tui::logo::TAGLINE),
        state.theme.muted(),
    )));
    lines.extend(vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Welcome to the DegenBox HL Signer.",
            state.theme.neutral().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  This program holds the only copy of your Hyperliquid",
            state.theme.neutral(),
        )),
        Line::from(Span::styled(
            "  API agent key. Trades the DegenBox server queues for you",
            state.theme.neutral(),
        )),
        Line::from(Span::styled(
            "  are signed LOCALLY and POSTed to Hyperliquid directly.",
            state.theme.neutral(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  The server NEVER sees your private key.",
            state.theme.warn().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Press Enter to continue, Esc to quit.",
            state.theme.muted(),
        )),
    ]);
    lines
}

fn render_key_source(state: &State) -> Vec<Line<'static>> {
    let gen_style = if state.selected_source == KeySource::Generate {
        state.theme.tab_active()
    } else {
        state.theme.tab_inactive()
    };
    let imp_style = if state.selected_source == KeySource::Import {
        state.theme.tab_active()
    } else {
        state.theme.tab_inactive()
    };
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(" Generate ", gen_style),
            Span::raw("    "),
            Span::styled(" Import   ", imp_style),
            Span::raw("    "),
            Span::styled("(Tab/Left/Right to switch)", state.theme.muted()),
        ]),
        Line::from(""),
    ];
    match state.selected_source {
        KeySource::Generate => {
            if state.show_generated {
                lines.push(Line::from(Span::styled(
                    "  Generated 32-byte secret (write it down!):",
                    state.theme.neutral(),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        state.private_key_hex.clone(),
                        state.theme.warn().add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "  Press Enter when you have it backed up.",
                    state.theme.muted(),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "  Press g to generate a fresh 32-byte secret,",
                    state.theme.neutral(),
                )));
                lines.push(Line::from(Span::styled(
                    "  or just Enter to do the same and continue.",
                    state.theme.neutral(),
                )));
            }
        }
        KeySource::Import => {
            lines.push(Line::from(Span::styled(
                "  Paste your existing 32-byte secp256k1 private key (hex):",
                state.theme.neutral(),
            )));
            lines.push(Line::from(""));
            let mask = "*".repeat(state.private_key_hex.chars().count());
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(mask, state.theme.warn().add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  Press Enter to continue.",
                state.theme.muted(),
            )));
        }
    }
    lines
}

fn render_backend(state: &State) -> Vec<Line<'static>> {
    let f_style = if state.selected_backend == Backend::File {
        state.theme.tab_active()
    } else {
        state.theme.tab_inactive()
    };
    let k_style = if state.selected_backend == Backend::Keyring {
        state.theme.tab_active()
    } else {
        state.theme.tab_inactive()
    };
    let pass_label = if state.pass_focus { " " } else { ">" };
    let conf_label = if state.pass_focus { ">" } else { " " };
    vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(" File ", f_style),
            Span::raw("    "),
            Span::styled(" OS keychain ", k_style),
            Span::raw("    "),
            Span::styled(
                "(file is recommended today — keyring lands next sprint)",
                state.theme.muted(),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Encryption: Argon2id (t=3, m=64MB, p=4) + AES-256-GCM",
            state.theme.muted(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(pass_label.to_string(), state.theme.accent()),
            Span::raw(" passphrase    "),
            Span::styled(
                "*".repeat(state.passphrase.chars().count()),
                state.theme.neutral(),
            ),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(conf_label.to_string(), state.theme.accent()),
            Span::raw(" confirm       "),
            Span::styled(
                "*".repeat(state.confirm_passphrase.chars().count()),
                state.theme.neutral(),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Up/Down to switch field, Enter to encrypt + save.",
            state.theme.muted(),
        )),
    ]
}

fn render_account(state: &State) -> Vec<Line<'static>> {
    let acct_label = if state.account_focus_name { " " } else { ">" };
    let name_label = if state.account_focus_name { ">" } else { " " };
    let acct_display = if state.account_address.is_empty() {
        Span::styled("0x… (paste your HL master wallet)", state.theme.muted())
    } else {
        Span::styled(
            state.account_address.clone(),
            state.theme.neutral().add_modifier(Modifier::BOLD),
        )
    };
    vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Your HL MASTER wallet — the 0x… account that holds your funds.",
            state.theme.neutral(),
        )),
        Line::from(Span::styled(
            "  This is REQUIRED: the server only delivers trades to a paired",
            state.theme.neutral(),
        )),
        Line::from(Span::styled(
            "  signer. It is NOT the agent address you imported.",
            state.theme.warn(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(acct_label.to_string(), state.theme.accent()),
            Span::raw(" account  "),
            acct_display,
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(name_label.to_string(), state.theme.accent()),
            Span::raw(" name     "),
            Span::styled(state.client_name.clone(), state.theme.neutral()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Up/Down to switch field, Enter to continue.",
            state.theme.muted(),
        )),
    ]
}

fn render_register(state: &State) -> Vec<Line<'static>> {
    let token_view = state.register_token.clone();
    let status_line = match &state.register_status {
        RegisterStatus::Idle => Line::from(""),
        RegisterStatus::InFlight => Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "\u{2807} registering with ",
                state.theme.accent().add_modifier(Modifier::BOLD),
            ),
            Span::styled(state.server_url.clone(), state.theme.neutral()),
            Span::styled("  (this can take a few seconds)", state.theme.muted()),
        ]),
        RegisterStatus::Ok { user_id, agent } => Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "\u{2713} registered ",
                state.theme.ok().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("user {user_id} / agent {agent}"),
                state.theme.neutral(),
            ),
        ]),
        RegisterStatus::Failed(err) => Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "\u{2717} failed ",
                state.theme.err().add_modifier(Modifier::BOLD),
            ),
            Span::styled(err.clone(), state.theme.err()),
        ]),
    };
    vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Paste a registration token from the DegenBox dashboard",
            state.theme.neutral(),
        )),
        Line::from(Span::styled(
            "  (Account → API tokens → New registration token).",
            state.theme.neutral(),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("token: ", state.theme.muted()),
            Span::styled(
                token_view,
                state.theme.neutral().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        status_line,
        Line::from(""),
        Line::from(Span::styled(
            "  Enter to register, s to skip (you can register later).",
            state.theme.muted(),
        )),
    ]
}

fn render_done(state: &State) -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Setup complete!",
            state.theme.ok().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  The TUI will now open. From there you can start the",
            state.theme.neutral(),
        )),
        Line::from(Span::styled(
            "  bot, watch live orders, and inspect the keystore.",
            state.theme.neutral(),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Press Enter to continue.",
            state.theme.muted(),
        )),
    ]
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, state: &State) {
    let p = Paragraph::new(Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "[Enter] next  [Esc] quit  [Tab/Left/Right] switch  [Up/Down] field",
            state.theme.muted(),
        ),
    ]))
    .alignment(Alignment::Left);
    frame.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tempfile::tempdir;

    fn fresh_state() -> State {
        let dir = tempdir().unwrap().keep();
        State {
            step: Step::Welcome,
            theme: Theme::plain(),
            selected_source: KeySource::Generate,
            selected_backend: Backend::File,
            private_key_hex: String::new(),
            passphrase: String::new(),
            confirm_passphrase: String::new(),
            pass_focus: false,
            account_address: String::new(),
            client_name: "test-client".into(),
            account_focus_name: false,
            register_token: String::new(),
            register_status: RegisterStatus::Idle,
            error: None,
            server_url: "http://localhost".into(),
            ks_path: dir.join("ks.json"),
            cfg_path: dir.join("cfg.json"),
            show_generated: false,
        }
    }

    #[test]
    fn renders_each_step_without_panic() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        for step in [
            Step::Welcome,
            Step::KeySource,
            Step::Backend,
            Step::Account,
            Step::Register,
            Step::Done,
        ] {
            let mut s = fresh_state();
            s.step = step;
            terminal.draw(|f| render(f, &s)).unwrap();
        }
    }

    #[test]
    fn welcome_step_mentions_self_custody() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let s = fresh_state();
        terminal.draw(|f| render(f, &s)).unwrap();
        let dump = dump(terminal.backend().buffer());
        assert!(dump.to_lowercase().contains("private key"));
    }

    fn dump(buf: &ratatui::buffer::Buffer) -> String {
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }
}
