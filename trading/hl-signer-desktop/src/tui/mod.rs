//! Ratatui-based interactive TUI for the HL signer.
//!
//! The default invocation (`hl-signer-desktop` with no args) opens
//! this. Power users (the legacy Go-bot Bubbletea crowd) get a
//! ratatui screen with live status, queue feed, key management,
//! settings and tailing logs. Headless / systemd installs keep using
//! the plain `daemon` subcommand — the TUI is purely additive.
//!
//! Module layout:
//!
//! - [`app`]          — `App` state machine: active tab, focus,
//!                       in-flight modal, runtime data the screens
//!                       project from.
//! - [`screens`]      — one module per tab (status, wallet, settings,
//!                       logs). Each exposes a pure `render(&App,
//!                       Frame, Rect)` plus a `handle_key` hook.
//! - [`widgets`]      — small reusable bits (status pill, key-value
//!                       row, log line, modal frame). Easier to
//!                       snapshot-test in isolation than the full
//!                       screen.
//! - [`setup_wizard`] — first-run, 4-step wizard. Replaces the
//!                       `setup` subcommand when launched from the
//!                       TUI; the subcommand still works.
//! - [`theme`]        — color tokens. Respects `NO_COLOR=1`.
//! - [`log_capture`]  — `tracing` layer that buffers the last N lines
//!                       in a ring so the Logs tab can tail them.
//! - [`events`]       — terminal event stream + tick generator.

pub mod app;
pub mod events;
pub mod log_capture;
pub mod screens;
pub mod setup_wizard;
pub mod theme;
pub mod widgets;

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::config;
use app::{App, AppOutcome, Tab};

/// Run the TUI to completion. Returns when the user quits.
///
/// `tick` is the cadence at which a Tick event is fired even when
/// nothing else is happening. The Status tab uses it to refresh the
/// uptime counter and re-poll the daemon state.
pub async fn run() -> Result<()> {
    // Bootstrap: figure out whether the user has a keystore yet. If
    // not we land in the first-run wizard; otherwise the main screen.
    let ks_path = config::default_keystore_path().context("default keystore path")?;
    let cfg_path = config::default_config_path().context("default config path")?;
    let has_keystore = ks_path.exists();

    let mut terminal = setup_terminal().context("init terminal")?;
    let log_buf = log_capture::install();

    let result = if has_keystore {
        run_main(&mut terminal, ks_path, cfg_path, log_buf).await
    } else {
        // Wizard first — once it completes we fall through into the
        // main app on the same terminal handle.
        let wiz_result = setup_wizard::run(&mut terminal, ks_path.clone(), cfg_path.clone()).await;
        match wiz_result {
            Ok(setup_wizard::WizardOutcome::Completed) => {
                run_main(&mut terminal, ks_path, cfg_path, log_buf).await
            }
            Ok(setup_wizard::WizardOutcome::Quit) => Ok(()),
            Err(e) => Err(e),
        }
    };

    restore_terminal(&mut terminal).ok();
    result
}

async fn run_main(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ks_path: std::path::PathBuf,
    cfg_path: std::path::PathBuf,
    log_buf: log_capture::LogBuffer,
) -> Result<()> {
    let mut app = App::bootstrap(ks_path, cfg_path, log_buf)?;
    let mut events = events::EventStream::new(Duration::from_millis(250));

    loop {
        terminal.draw(|f| app.render(f))?;
        if let Some(ev) = events.next().await {
            match ev {
                events::TuiEvent::Tick => app.on_tick(),
                events::TuiEvent::Term(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                    let outcome = match (k.code, k.modifiers) {
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => AppOutcome::Quit,
                        _ => app.on_key(k.code, k.modifiers),
                    };
                    if matches!(outcome, AppOutcome::Quit) {
                        return Ok(());
                    }
                }
                events::TuiEvent::Term(Event::Mouse(m))
                    if m.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left) =>
                {
                    app.on_mouse(m.column, m.row);
                }
                events::TuiEvent::Term(Event::Resize(_, _)) => {
                    // ratatui auto-relayouts on the next draw; nothing
                    // to do beyond letting the loop redraw.
                }
                _ => {}
            }
        }
    }
}

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    Terminal::new(CrosstermBackend::new(out))
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Convenience re-exports for binaries that wire the TUI in directly.
pub use app::App as TuiApp;
pub use setup_wizard::WizardOutcome;

/// Order in which tabs appear in the header. Kept here so the click
/// handler and the renderer agree on indexing.
pub const TAB_ORDER: &[Tab] = &[Tab::Status, Tab::Wallet, Tab::Settings, Tab::Logs];
