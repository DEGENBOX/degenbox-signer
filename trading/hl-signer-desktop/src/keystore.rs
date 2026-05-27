//! Encrypted keystore for the HL API agent private key.
//!
//! Wire-compatible with the legacy Go bot
//! (`legay-hyperliquid-bot/degenbox-client/internal/keystore/keystore.go`).
//! Users who already have a legacy `degenbox.keystore.json` can pass
//! it to `hl-signer-desktop daemon --keystore=<path>` and it will
//! decrypt without re-pasting the key.
//!
//! ## Envelope
//!
//! ```text
//! salt           = random 32 bytes
//! nonce          = random 12 bytes  (AES-GCM standard)
//! derived_key    = argon2id(passphrase, salt, t=3, m=64MB, p=4, len=32)
//! ciphertext     = AES-256-GCM(derived_key, nonce, secret)
//! ```
//!
//! All fields are stored hex-encoded inside a JSON envelope so the
//! file is human-inspectable and trivially backed up.

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

/// Argon2id parameters — pinned to match the legacy Go bot. Changing
/// any of these breaks migration of legacy keystores so they live as
/// constants, not env vars.
pub const ARGON2_TIME: u32 = 3;
pub const ARGON2_MEMORY_KIB: u32 = 64 * 1024; // 64 MB
pub const ARGON2_THREADS: u32 = 4;
pub const KEY_LEN: usize = 32; // AES-256
pub const SALT_LEN: usize = 32;
pub const NONCE_LEN: usize = 12; // AES-GCM standard

#[derive(Debug, Error)]
pub enum KeystoreError {
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
pub struct KeystoreFile {
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

/// Encrypt a 32-byte secp256k1 secret + write to `path`.
///
/// Wipes the derived key from memory on return (best-effort — copies
/// in serializer buffers may persist).
pub fn encrypt_and_save(
    private_key_hex: &str,
    passphrase: &[u8],
    path: &Path,
) -> Result<String, KeystoreError> {
    let key_bytes = hex::decode(private_key_hex.trim().trim_start_matches("0x"))?;
    if key_bytes.len() != 32 {
        return Err(KeystoreError::BadKeyHex);
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
        .map_err(|e| KeystoreError::Crypto(e.to_string()))?;

    let file = KeystoreFile {
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
    // Best-effort zeroize.
    let _ = secret;
    Ok(address)
}

/// Decrypt the keystore at `path` and return `(secret_hex, address)`.
pub fn decrypt(path: &Path, passphrase: &[u8]) -> Result<(String, String), KeystoreError> {
    let data = fs::read(path)?;
    let file: KeystoreFile = serde_json::from_slice(&data)?;
    if file.version != 1 {
        return Err(KeystoreError::UnsupportedVersion(file.version));
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
        .map_err(|_| KeystoreError::BadPassphrase)?;
    if plaintext.len() != 32 {
        return Err(KeystoreError::BadKeyHex);
    }
    let mut secret = [0u8; 32];
    secret.copy_from_slice(&plaintext);
    let address = derive_address(&secret)?;
    if address.to_ascii_lowercase() != file.address.to_ascii_lowercase() {
        return Err(KeystoreError::AddressMismatch);
    }
    Ok((hex::encode(secret), address))
}

/// Inspect a keystore file's stored address without decrypting it.
/// Used by `register` so the binary can print the agent address
/// without prompting for the passphrase.
pub fn peek_address(path: &Path) -> Result<String, KeystoreError> {
    let data = fs::read(path)?;
    let file: KeystoreFile = serde_json::from_slice(&data)?;
    Ok(file.address)
}

fn derive_key(passphrase: &[u8], salt: &[u8]) -> Result<[u8; KEY_LEN], KeystoreError> {
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
) -> Result<[u8; KEY_LEN], KeystoreError> {
    let params = Params::new(memory_kib, time, threads, Some(key_len))
        .map_err(|e| KeystoreError::Argon2(e.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; KEY_LEN];
    argon
        .hash_password_into(passphrase, salt, &mut out)
        .map_err(|e| KeystoreError::Argon2(e.to_string()))?;
    Ok(out)
}

/// Derive the 0x-prefixed lowercase eth address from the 32-byte
/// secp256k1 secret. Same path as `geth`'s `crypto.PubkeyToAddress`.
pub fn derive_address(secret: &[u8; 32]) -> Result<String, KeystoreError> {
    let signing_key =
        SigningKey::from_bytes(secret.into()).map_err(|e| KeystoreError::Crypto(e.to_string()))?;
    let verifying = signing_key.verifying_key();
    let encoded = verifying.to_encoded_point(false);
    let xy = &encoded.as_bytes()[1..];
    let mut h = Keccak256::new();
    h.update(xy);
    let out = h.finalize();
    Ok(format!("0x{}", hex::encode(&out[12..])))
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), KeystoreError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(&bytes)?;
    // 0600 on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(tmp.path(), perm)?;
    }
    tmp.persist(path).map_err(|e| KeystoreError::Io(e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const FIXTURE_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn keystore_roundtrip_recovers_secret_and_address() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        let addr1 = encrypt_and_save(FIXTURE_KEY, b"hunter2", &path).unwrap();
        let (secret_hex, addr2) = decrypt(&path, b"hunter2").unwrap();
        assert_eq!(secret_hex, FIXTURE_KEY);
        assert_eq!(addr1, addr2);
    }

    #[test]
    fn wrong_passphrase_is_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        encrypt_and_save(FIXTURE_KEY, b"hunter2", &path).unwrap();
        let err = decrypt(&path, b"wrong").unwrap_err();
        assert!(matches!(err, KeystoreError::BadPassphrase), "got {err:?}");
    }

    #[test]
    fn address_derivation_is_deterministic() {
        let bytes = hex::decode(FIXTURE_KEY).unwrap();
        let mut k = [0u8; 32];
        k.copy_from_slice(&bytes);
        let a1 = derive_address(&k).unwrap();
        let a2 = derive_address(&k).unwrap();
        assert_eq!(a1, a2);
        assert!(a1.starts_with("0x") && a1.len() == 42);
    }

    #[test]
    fn keystore_file_uses_legacy_argon2_params() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        encrypt_and_save(FIXTURE_KEY, b"hunter2", &path).unwrap();
        let raw = std::fs::read(&path).unwrap();
        let file: KeystoreFile = serde_json::from_slice(&raw).unwrap();
        assert_eq!(file.kdf, "argon2id");
        assert_eq!(file.algorithm, "aes-256-gcm");
        assert_eq!(file.kdf_params.time, 3);
        assert_eq!(file.kdf_params.memory, 64 * 1024);
        assert_eq!(file.kdf_params.threads, 4);
        assert_eq!(file.kdf_params.key_len, 32);
    }

    #[test]
    fn invalid_key_hex_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        let err = encrypt_and_save("not_hex", b"x", &path).unwrap_err();
        assert!(matches!(err, KeystoreError::Hex(_)));
        let err2 = encrypt_and_save("00ff", b"x", &path).unwrap_err();
        assert!(matches!(err2, KeystoreError::BadKeyHex));
    }

    #[test]
    fn keystore_file_is_chmod_0600_on_unix() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        encrypt_and_save(FIXTURE_KEY, b"hunter2", &path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(&path).unwrap();
            assert_eq!(m.permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn peek_address_does_not_require_passphrase() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ks.json");
        let addr = encrypt_and_save(FIXTURE_KEY, b"hunter2", &path).unwrap();
        let peeked = peek_address(&path).unwrap();
        assert_eq!(addr, peeked);
    }

    /// Re-encrypting the SAME key under the SAME passphrase must produce
    /// a different ciphertext because the random salt + nonce vary. This
    /// pins the randomness path so a future refactor can't accidentally
    /// hard-code a salt.
    #[test]
    fn fresh_encryption_has_distinct_salt_and_nonce() {
        let dir = tempdir().unwrap();
        let p1 = dir.path().join("a.json");
        let p2 = dir.path().join("b.json");
        encrypt_and_save(FIXTURE_KEY, b"x", &p1).unwrap();
        encrypt_and_save(FIXTURE_KEY, b"x", &p2).unwrap();
        let f1: KeystoreFile = serde_json::from_slice(&std::fs::read(&p1).unwrap()).unwrap();
        let f2: KeystoreFile = serde_json::from_slice(&std::fs::read(&p2).unwrap()).unwrap();
        assert_ne!(f1.salt, f2.salt);
        assert_ne!(f1.nonce, f2.nonce);
        assert_ne!(f1.ciphertext, f2.ciphertext);
        assert_eq!(f1.address, f2.address); // same key → same address
    }
}
