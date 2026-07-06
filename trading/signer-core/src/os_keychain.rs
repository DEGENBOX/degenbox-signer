//! OS-keychain integration for the encrypted keystore passphrase.
//!
//! Strategy: we keep the existing AES-GCM-encrypted keystore file as
//! the source of truth (so a copied-out file is still useless without
//! the passphrase) BUT the passphrase itself goes into the OS
//! credential store. The desktop app reads it back without prompting
//! the user on each start.
//!
//! Backends:
//!
//! - macOS — `security-framework` → Keychain Services API
//! - Windows — Win32 `CredRead`/`CredWrite`
//! - Linux — `secret-service` (gnome-keyring, kwallet5/6, KeePassXC)
//!
//! All three are abstracted by the `keyring` crate (well-maintained,
//! widely audited, used by `cargo-credential-1password` upstream).
//! Falling back to file-only is a runtime decision the user makes in
//! onboarding step 2.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OsKeychainError {
    #[error("keychain unavailable on this platform: {0}")]
    Unavailable(String),
    #[error("entry not found")]
    NotFound,
    #[error("keyring backend: {0}")]
    Backend(String),
}

/// Which storage backend the user picked during onboarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeystoreBackend {
    /// Encrypted-file-only; user types passphrase on every start.
    File,
    /// Encrypted file + passphrase cached in the OS credential store.
    /// File still encrypted at rest so a copied file is useless.
    Keychain,
}

/// Service identifier we register under in the OS credential store.
/// All passphrases for one user (HL agent + Solana hot-wallet) share
/// the same service and disambiguate by `account`.
pub const SERVICE: &str = "app.degenbox.signer";

#[cfg(feature = "keychain")]
pub fn store(account: &str, secret: &str) -> Result<(), OsKeychainError> {
    let entry = keyring::Entry::new(SERVICE, account)
        .map_err(|e| OsKeychainError::Backend(e.to_string()))?;
    entry
        .set_password(secret)
        .map_err(|e| OsKeychainError::Backend(e.to_string()))
}

#[cfg(feature = "keychain")]
pub fn load(account: &str) -> Result<String, OsKeychainError> {
    let entry = keyring::Entry::new(SERVICE, account)
        .map_err(|e| OsKeychainError::Backend(e.to_string()))?;
    entry.get_password().map_err(|e| match e {
        keyring::Error::NoEntry => OsKeychainError::NotFound,
        other => OsKeychainError::Backend(other.to_string()),
    })
}

#[cfg(feature = "keychain")]
pub fn delete(account: &str) -> Result<(), OsKeychainError> {
    let entry = keyring::Entry::new(SERVICE, account)
        .map_err(|e| OsKeychainError::Backend(e.to_string()))?;
    entry.delete_credential().map_err(|e| match e {
        keyring::Error::NoEntry => OsKeychainError::NotFound,
        other => OsKeychainError::Backend(other.to_string()),
    })
}

// Stubs for when the feature is off so consumers can still compile
// the `KeystoreBackend` enum + branch on it at runtime.

#[cfg(not(feature = "keychain"))]
pub fn store(_account: &str, _secret: &str) -> Result<(), OsKeychainError> {
    Err(OsKeychainError::Unavailable(
        "build the desktop app with --features keychain".into(),
    ))
}

#[cfg(not(feature = "keychain"))]
pub fn load(_account: &str) -> Result<String, OsKeychainError> {
    Err(OsKeychainError::Unavailable(
        "build the desktop app with --features keychain".into(),
    ))
}

#[cfg(not(feature = "keychain"))]
pub fn delete(_account: &str) -> Result<(), OsKeychainError> {
    Err(OsKeychainError::Unavailable(
        "build the desktop app with --features keychain".into(),
    ))
}
