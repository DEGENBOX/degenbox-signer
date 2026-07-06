//! Per-tab renderers and key handlers.

use crossterm::event::{KeyCode, KeyModifiers};

use super::app::{App, AppOutcome, Modal, Tab};

pub mod clients;
pub mod logs;
pub mod settings;
pub mod solana;
pub mod status;
pub mod wallet;

/// Dispatch a keystroke to the active tab's handler. Common
/// keystrokes (Tab, q, 1-6, p) are handled by the parent before this
/// is called.
pub fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> AppOutcome {
    match app.tab {
        Tab::Status => status::handle_key(app, code, mods),
        Tab::Wallet => wallet::handle_key(app, code, mods),
        Tab::Solana => solana::handle_key(app, code, mods),
        Tab::Clients => clients::handle_key(app, code, mods),
        Tab::Settings => settings::handle_key(app, code, mods),
        Tab::Logs => logs::handle_key(app, code, mods),
    }
}

/// Helper used by Wallet to open the unlock modal.
pub fn open_unlock_modal(app: &mut App) {
    app.modal = Some(Modal::Unlock {
        input: String::new(),
        error: None,
    });
}
