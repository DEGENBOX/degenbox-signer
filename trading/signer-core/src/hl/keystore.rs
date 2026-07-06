//! Hyperliquid agent-key primitives.
//!
//! Extracted from `hl-signer-desktop/src/keystore.rs` + `signing.rs`
//! so the same primitives back the CLI binary, the Tauri desktop app,
//! and (later) any other transport that needs to hold an HL agent
//! key locally.
//!
//! ## What lives here
//!
//! - [`HlKeystore`] — encrypted on-disk format wire-compatible with
//!   the legacy Go bot AND the v1 `hl-signer-desktop` envelope
//!   (Argon2id t=3 / m=64 MiB / p=4 + AES-256-GCM).
//! - [`derive_address`] — 0x-prefixed eth address from a 32-byte
//!   secp256k1 secret. Same path as `geth`'s `PubkeyToAddress`.
//! - [`load`] / [`save`] / [`peek_address`] — disk I/O with 0600
//!   permissions on Unix + atomic write via tempfile rename.
//!
//! The actual `/exchange` transport (placing orders, cancels,
//! updateLeverage, vault transfer) stays in `platform-hl-exchange`
//! which the consumer binaries pull in directly — duplicating that
//! here would mean re-implementing the EIP-712 type system from
//! scratch.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use k256::ecdsa::SigningKey;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};
use std::fs;
use std::io::Write;
use std::path::Path;
use thiserror::Error;

/// Argon2id parameters — pinned to match the legacy Go bot. Any
/// change here breaks migration of pre-existing keystores so they
/// stay constants, not env vars.
pub const ARGON2_TIME: u32 = 3;
pub const ARGON2_MEMORY_KIB: u32 = 64 * 1024;
pub const ARGON2_THREADS: u32 = 4;
pub const KEY_LEN: usize = 32;
pub const SALT_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;

#[derive(Debug, Error)]
pub enum HlKeystoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hex: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("argon2: {0}")]
    Argon2(String),
    #[error("aes-gcm: {0}")]
    Crypto(String),
    #[error("unsupported keystore version {0} (expected 1)")]
    UnsupportedVersion(u32),
    #[error("invalid private key: must be 32 bytes hex")]
    BadKeyHex,
    #[error("address mismatch after decryption — keystore is corrupt")]
    AddressMismatch,
    #[error("decryption failed (wrong passphrase?)")]
    BadPassphrase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlKeystore {
    pub version: u32,
    pub algorithm: String,
    pub kdf: String,
    pub kdf_params: KdfParams,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KdfParams {
    pub time: u32,
    pub memory: u32,
    pub threads: u8,
    pub key_len: u32,
}

/// Encrypt a 32-byte secp256k1 secret + write the keystore to `path`.
/// Returns the derived 0x-prefixed eth address.
pub fn save(
    private_key_hex: &str,
    passphrase: &[u8],
    path: &Path,
) -> Result<String, HlKeystoreError> {
    let key_bytes = hex::decode(private_key_hex.trim().trim_start_matches("0x"))?;
    if key_bytes.len() != 32 {
        return Err(HlKeystoreError::BadKeyHex);
    }
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&key_bytes);
    let address = derive_address(&secret)?;

    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);

    let derived = derive_key(passphrase, &salt)?;
    let cipher_key = Key::<Aes256Gcm>::from_slice(&derived);
    let cipher = Aes256Gcm::new(cipher_key);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, secret.as_ref())
        .map_err(|e| HlKeystoreError::Crypto(e.to_string()))?;

    let file = HlKeystore {
        version: 1,
        algorithm: "aes-256-gcm".into(),
        kdf: "argon2id".into(),
        kdf_params: KdfParams {
            time: ARGON2_TIME,
            memory: ARGON2_MEMORY_KIB,
            threads: ARGON2_THREADS as u8,
            key_len: KEY_LEN as u32,
        },
        salt: hex::encode(salt),
        nonce: hex::encode(nonce_bytes),
        ciphertext: hex::encode(ciphertext),
        address: address.clone(),
    };
    atomic_write_json(path, &file)?;
    Ok(address)
}

/// Load + decrypt the keystore at `path`. Returns `(secret_hex, address)`.
pub fn load(path: &Path, passphrase: &[u8]) -> Result<(String, String), HlKeystoreError> {
    let data = fs::read(path)?;
    let file: HlKeystore = serde_json::from_slice(&data)?;
    if file.version != 1 {
        return Err(HlKeystoreError::UnsupportedVersion(file.version));
    }
    let salt = hex::decode(&file.salt)?;
    let nonce_bytes = hex::decode(&file.nonce)?;
    let ciphertext = hex::decode(&file.ciphertext)?;

    let derived = derive_key_with_params(
        passphrase,
        &salt,
        file.kdf_params.time,
        file.kdf_params.memory,
        file.kdf_params.threads as u32,
        file.kdf_params.key_len as usize,
    )?;
    let cipher_key = Key::<Aes256Gcm>::from_slice(&derived);
    let cipher = Aes256Gcm::new(cipher_key);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let plaintext = cipher
        .decrypt(nonce, ciphertext.as_ref())
        .map_err(|_| HlKeystoreError::BadPassphrase)?;
    if plaintext.len() != 32 {
        return Err(HlKeystoreError::BadKeyHex);
    }
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&plaintext);
    let address = derive_address(&secret)?;
    if !address.eq_ignore_ascii_case(&file.address) {
        return Err(HlKeystoreError::AddressMismatch);
    }
    Ok((hex::encode(secret), address))
}

/// Inspect the stored address without decrypting. Used by the daemon +
/// UI status surface so we never prompt for a passphrase just to display
/// the agent address.
pub fn peek_address(path: &Path) -> Result<String, HlKeystoreError> {
    let data = fs::read(path)?;
    let file: HlKeystore = serde_json::from_slice(&data)?;
    Ok(file.address)
}

fn derive_key(passphrase: &[u8], salt: &[u8]) -> Result<[u8; KEY_LEN], HlKeystoreError> {
    derive_key_with_params(
        passphrase,
        salt,
        ARGON2_TIME,
        ARGON2_MEMORY_KIB,
        ARGON2_THREADS,
        KEY_LEN,
    )
}

fn derive_key_with_params(
    passphrase: &[u8],
    salt: &[u8],
    time: u32,
    memory_kib: u32,
    threads: u32,
    key_len: usize,
) -> Result<[u8; KEY_LEN], HlKeystoreError> {
    let params = Params::new(memory_kib, time, threads, Some(key_len))
        .map_err(|e| HlKeystoreError::Argon2(e.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase, salt, &mut out)
        .map_err(|e| HlKeystoreError::Argon2(e.to_string()))?;
    Ok(out)
}

/// Derive the 0x-prefixed lowercase eth address from a 32-byte
/// secp256k1 secret. Same path as `geth`'s `crypto.PubkeyToAddress`:
/// keccak256(uncompressed_pubkey_xy)[12..].
pub fn derive_address(secret: &[u8; 32]) -> Result<String, HlKeystoreError> {
    let signing_key = SigningKey::from_bytes(secret.into())
        .map_err(|e| HlKeystoreError::Crypto(e.to_string()))?;
    let verifying = signing_key.verifying_key();
    let encoded = verifying.to_encoded_point(false);
    let xy = &encoded.as_bytes()[1..];
    let mut h = Keccak256::new();
    h.update(xy);
    let out = h.finalize();
    Ok(format!("0x{}", hex::encode(&out[12..])))
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), HlKeystoreError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    // Atomic write: create a tempfile in the same dir, write + fsync
    // (via Drop), rename into place. Crash mid-write can't corrupt
    // the user's only copy of their (encrypted) private key.
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(&bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(tmp.path(), perm)?;
    }
    tmp.persist(path)
        .map_err(|e| HlKeystoreError::Io(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const FIXTURE_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn hl_keystore_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        let addr1 = save(FIXTURE_KEY, b"hunter2", &path).unwrap();
        let (secret_hex, addr2) = load(&path, b"hunter2").unwrap();
        assert_eq!(secret_hex, FIXTURE_KEY);
        assert_eq!(addr1, addr2);
        assert!(addr1.starts_with("0x") && addr1.len() == 42);
    }

    #[test]
    fn wrong_passphrase_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        save(FIXTURE_KEY, b"hunter2", &path).unwrap();
        let err = load(&path, b"wrong").unwrap_err();
        assert!(matches!(err, HlKeystoreError::BadPassphrase), "got {err:?}");
    }

    #[test]
    fn address_derivation_deterministic() {
        let bytes = hex::decode(FIXTURE_KEY).unwrap();
        let mut k = [0u8; 32];
        k.copy_from_slice(&bytes);
        let a1 = derive_address(&k).unwrap();
        let a2 = derive_address(&k).unwrap();
        assert_eq!(a1, a2);
    }

    #[test]
    fn peek_does_not_need_passphrase() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        let addr = save(FIXTURE_KEY, b"hunter2", &path).unwrap();
        let peeked = peek_address(&path).unwrap();
        assert_eq!(addr, peeked);
    }

    #[test]
    fn legacy_argon2_params_pinned() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        save(FIXTURE_KEY, b"hunter2", &path).unwrap();
        let raw = std::fs::read(&path).unwrap();
        let file: HlKeystore = serde_json::from_slice(&raw).unwrap();
        assert_eq!(file.kdf, "argon2id");
        assert_eq!(file.algorithm, "aes-256-gcm");
        assert_eq!(file.kdf_params.time, 3);
        assert_eq!(file.kdf_params.memory, 64 * 1024);
        assert_eq!(file.kdf_params.threads, 4);
        assert_eq!(file.kdf_params.key_len, 32);
    }
}
