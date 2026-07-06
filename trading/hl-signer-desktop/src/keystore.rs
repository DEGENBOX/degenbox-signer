//! Encrypted keystore for the HL API agent private key — re-pointed
//! onto the canonical implementation in `signer-core`.
//!
//! The byte-identical envelope (Argon2id t=3 / m=64 MiB / p=4 +
//! AES-256-GCM, hex-encoded JSON fields, wire-compatible with the
//! legacy Go bot) used to live here; it moved to
//! `degenbox_signer_core::hl::keystore` during the Wave-4 dedupe and
//! this module keeps the CLI's historical names so every call site
//! (setup wizard, daemon unlock, panic, migrate) reads unchanged:
//!
//! - `encrypt_and_save` → core `save`
//! - `decrypt`          → core `load`
//! - `KeystoreError`    → core `HlKeystoreError`

pub use degenbox_signer_core::hl::{
    derive_address, load as decrypt, peek_address, save as encrypt_and_save,
    HlKeystoreError as KeystoreError,
};
