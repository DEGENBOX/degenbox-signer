//! DegenBox gateway client — re-pointed onto the canonical copy in
//! `signer-core` (`hl::server`, wire-identical: register / redeem /
//! pending / order-result / verify-totp).
//!
//! The CLI keeps exactly one local extra: the blocking stdin TOTP
//! prompt, which is terminal-specific and deliberately did NOT move
//! into the shared crate (GUI hosts park a `TotpPrompt` in the shared
//! runtime instead).

pub use degenbox_signer_core::hl::server::{
    RedeemRegistrationReq, RegisterReq, ServerClient, ServerError,
};

use tracing::warn;

/// Prompt the user for a TOTP code via stdin, blocking the calling thread.
/// Must be called from `tokio::task::spawn_blocking` to avoid blocking the
/// async executor.
pub fn prompt_totp_stdin(expires_at: &str) -> Option<String> {
    use std::io::{self, BufRead};
    eprintln!();
    eprintln!("  ╔══════════════════════════════════════════╗");
    eprintln!("  ║  2FA required by DegenBox gateway        ║");
    eprintln!("  ║  Challenge expires: {expires_at:<22}║");
    eprintln!("  ╚══════════════════════════════════════════╝");
    eprint!("  Enter authenticator code: ");
    let stdin = io::stdin();
    let mut line = String::new();
    match stdin.lock().read_line(&mut line) {
        Ok(0) | Err(_) => {
            warn!("stdin closed / unreadable — TOTP prompt skipped");
            None
        }
        Ok(_) => {
            let code = line.trim().to_string();
            if code.is_empty() {
                None
            } else {
                Some(code)
            }
        }
    }
}
