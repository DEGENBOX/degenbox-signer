//! Encrypted keystore for the signer's Solana keypair.
//!
//! ## Format
//!
//! On-disk JSON:
//! ```text
//! {
//!   "version": 1,
//!   "kdf": "argon2id",
//!   "argon2": { "m_cost": 19456, "t_cost": 2, "p_cost": 1 },
//!   "salt":  "<32 bytes hex>",      // Argon2 salt
//!   "nonce": "<12 bytes hex>",      // AES-GCM nonce
//!   "ciphertext": "<hex>",          // AES-256-GCM(secret || magic)
//!   "pubkey": "<base58>"            // unencrypted, useful for UI
//! }
//! ```
//!
//! The Argon2id parameters match the OWASP-recommended baseline (19 MiB
//! memory, 2 iterations, 1 parallel lane). The encrypted plaintext is
//! the 32-byte ed25519 secret + a 4-byte magic so a wrong password
//! always fails authentication (AES-GCM tag), never returns a wrong
//! key silently.
//!
//! ## Threat model
//!
//! Designed to resist offline brute force on the keystore file. NOT
//! resistant against an adversary who has live RAM access to the
//! signer process — that's the OS keychain's job. We `Zeroize` the
//! decrypted secret immediately after building the keypair so RAM
//! exposure is bounded to the time between decrypt and signer
//! construction.

use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use solana_sdk::signature::{Keypair, SeedDerivable, Signer};
use thiserror::Error;
use zeroize::Zeroize;

const KEYSTORE_VERSION: u8 = 1;
/// Magic bytes appended to the secret before encryption. AES-GCM's
/// authentication tag already detects ciphertext tampering, but a
/// known plaintext suffix gives us a second-line check that the
/// decryption "succeeded" beyond just the tag — defends against
/// future cipher choices that may not auth-encrypt.
const MAGIC: &[u8; 4] = b"DBX1";

const ARGON2_M_COST_KIB: u32 = 19_456; // 19 MiB
const ARGON2_T_COST: u32 = 2;
const ARGON2_P_COST: u32 = 1;

#[derive(Debug, Error)]
pub enum KeystoreError {
    #[error("unsupported keystore version {0} (this build supports {KEYSTORE_VERSION})")]
    UnsupportedVersion(u8),
    #[error("invalid keystore format: {0}")]
    InvalidFormat(String),
    #[error("password incorrect or file corrupt")]
    BadPassword,
    #[error("magic mismatch — file may be corrupt or wrong format")]
    BadMagic,
    #[error("argon2 derivation failed: {0}")]
    Kdf(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("hex: {0}")]
    Hex(#[from] hex::FromHexError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Argon2Params {
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for Argon2Params {
    fn default() -> Self {
        Self {
            m_cost: ARGON2_M_COST_KIB,
            t_cost: ARGON2_T_COST,
            p_cost: ARGON2_P_COST,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keystore {
    pub version: u8,
    pub kdf: String,
    pub argon2: Argon2Params,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
    pub pubkey: String,
}

/// Encrypt a fresh ed25519 secret (`secret_key_bytes`, 32 bytes) under
/// `password`. The pubkey is derived deterministically from the
/// secret and stored unencrypted for UI convenience.
pub fn encrypt(
    secret_key_bytes: &[u8; 32],
    pubkey_b58: &str,
    password: &str,
) -> Result<Keystore, KeystoreError> {
    let mut salt = [0u8; 32];
    OsRng.fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let mut derived = [0u8; 32];
    let params = Argon2Params::default();
    derive_key(password, &salt, &params, &mut derived)?;

    // plaintext = secret || MAGIC (well-defined post-decrypt sanity).
    let mut plaintext = [0u8; 32 + 4];
    plaintext[..32].copy_from_slice(secret_key_bytes);
    plaintext[32..].copy_from_slice(MAGIC);

    let cipher = Aes256Gcm::new(&derived.into());
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_slice())
        .map_err(|_| KeystoreError::InvalidFormat("aes-gcm encrypt failed".into()))?;

    derived.zeroize();
    plaintext.zeroize();

    Ok(Keystore {
        version: KEYSTORE_VERSION,
        kdf: "argon2id".into(),
        argon2: params,
        salt: hex::encode(salt),
        nonce: hex::encode(nonce_bytes),
        ciphertext: hex::encode(ciphertext),
        pubkey: pubkey_b58.to_string(),
    })
}

/// Decrypt to a `solana_sdk::Keypair`. The 32-byte secret is wiped
/// immediately after the keypair is constructed so RAM exposure is
/// minimal.
pub fn decrypt(ks: &Keystore, password: &str) -> Result<Keypair, KeystoreError> {
    if ks.version != KEYSTORE_VERSION {
        return Err(KeystoreError::UnsupportedVersion(ks.version));
    }
    if ks.kdf != "argon2id" {
        return Err(KeystoreError::InvalidFormat(format!(
            "unknown kdf {:?}",
            ks.kdf
        )));
    }

    let salt = hex::decode(&ks.salt)?;
    let nonce_bytes = hex::decode(&ks.nonce)?;
    let ciphertext = hex::decode(&ks.ciphertext)?;
    if nonce_bytes.len() != 12 {
        return Err(KeystoreError::InvalidFormat("nonce length != 12".into()));
    }

    let mut derived = [0u8; 32];
    derive_key(password, &salt, &ks.argon2, &mut derived)?;

    let cipher = Aes256Gcm::new(&derived.into());
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext.as_slice())
        .map_err(|_| KeystoreError::BadPassword)?;

    derived.zeroize();

    if plaintext.len() != 36 {
        return Err(KeystoreError::InvalidFormat(format!(
            "plaintext length {} != 36",
            plaintext.len()
        )));
    }
    if &plaintext[32..] != MAGIC {
        return Err(KeystoreError::BadMagic);
    }

    // solana_sdk::Keypair stores the 64-byte secret-then-public form,
    // but it can be built from just the 32-byte secret seed via
    // `from_seed`. Build, then verify the embedded pubkey matches the
    // declared one as a final sanity check.
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&plaintext[..32]);
    let kp = Keypair::from_seed(&seed)
        .map_err(|e| KeystoreError::InvalidFormat(format!("keypair from seed: {e}")))?;
    seed.zeroize();
    drop(plaintext);

    if kp.pubkey().to_string() != ks.pubkey {
        return Err(KeystoreError::InvalidFormat(
            "decrypted secret does not match stored pubkey".into(),
        ));
    }
    Ok(kp)
}

fn derive_key(
    password: &str,
    salt: &[u8],
    params: &Argon2Params,
    out: &mut [u8; 32],
) -> Result<(), KeystoreError> {
    let p = Params::new(params.m_cost, params.t_cost, params.p_cost, Some(32))
        .map_err(|e| KeystoreError::Kdf(e.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, p);
    argon2
        .hash_password_into(password.as_bytes(), salt, out)
        .map_err(|e| KeystoreError::Kdf(e.to_string()))?;
    Ok(())
}

/// Generate a fresh ed25519 keypair + immediately encrypt it under
/// `password`. Returns the keystore + the keypair so the caller can
/// use it without re-decrypting. The caller is responsible for
/// dropping the keypair when done so its in-memory bytes get wiped.
pub fn generate(password: &str) -> Result<(Keystore, Keypair), KeystoreError> {
    let kp = Keypair::new();
    let pub_b58 = kp.pubkey().to_string();
    let secret_bytes = kp.secret_bytes();
    let ks = encrypt(secret_bytes, &pub_b58, password)?;
    Ok((ks, kp))
}

// ─── extension-keystore migration ───────────────────────────────────

/// On-disk/clipboard JSON shape of the Chrome extension's keystore
/// (`trading/signer-extension/src/crypto/keystore.ts`). Same crypto as
/// the Rust format — Argon2id → AES-256-GCM over `secret || "DBX1"` —
/// but different field names + base64 (not hex) encodings.
#[derive(Debug, Deserialize)]
struct ExtensionKeystore {
    v: u8,
    pubkey: String,
    kdf: String,
    kdf_params: ExtensionKdfParams,
    cipher: String,
    nonce_b64: String,
    ct_b64: String,
}

#[derive(Debug, Deserialize)]
struct ExtensionKdfParams {
    m: u32,
    t: u32,
    p: u32,
    salt_b64: String,
}

/// Import a keystore exported from the DegenBox Chrome extension:
/// decrypt the extension-format blob with `password`, then re-encrypt
/// the 32-byte secret into the native Rust [`Keystore`] format under
/// the SAME password. Returns the re-encrypted keystore plus the
/// keypair so the caller can verify/derive without re-decrypting.
pub fn import_extension_json(
    json: &str,
    password: &str,
) -> Result<(Keystore, Keypair), KeystoreError> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;

    let ext: ExtensionKeystore = serde_json::from_str(json.trim())
        .map_err(|e| KeystoreError::InvalidFormat(format!("not an extension keystore: {e}")))?;
    if ext.v != 1 {
        return Err(KeystoreError::UnsupportedVersion(ext.v));
    }
    if ext.kdf != "argon2id" {
        return Err(KeystoreError::InvalidFormat(format!(
            "unknown kdf {:?}",
            ext.kdf
        )));
    }
    if ext.cipher != "aes-256-gcm" {
        return Err(KeystoreError::InvalidFormat(format!(
            "unknown cipher {:?}",
            ext.cipher
        )));
    }

    let salt = b64
        .decode(&ext.kdf_params.salt_b64)
        .map_err(|e| KeystoreError::InvalidFormat(format!("salt_b64: {e}")))?;
    let nonce_bytes = b64
        .decode(&ext.nonce_b64)
        .map_err(|e| KeystoreError::InvalidFormat(format!("nonce_b64: {e}")))?;
    let ciphertext = b64
        .decode(&ext.ct_b64)
        .map_err(|e| KeystoreError::InvalidFormat(format!("ct_b64: {e}")))?;
    if nonce_bytes.len() != 12 {
        return Err(KeystoreError::InvalidFormat("nonce length != 12".into()));
    }

    let params = Argon2Params {
        m_cost: ext.kdf_params.m,
        t_cost: ext.kdf_params.t,
        p_cost: ext.kdf_params.p,
    };
    let mut derived = [0u8; 32];
    derive_key(password, &salt, &params, &mut derived)?;

    let cipher = Aes256Gcm::new(&derived.into());
    let mut plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext.as_slice())
        .map_err(|_| KeystoreError::BadPassword)?;
    derived.zeroize();

    if plaintext.len() != 36 {
        return Err(KeystoreError::InvalidFormat(format!(
            "plaintext length {} != 36",
            plaintext.len()
        )));
    }
    if &plaintext[32..] != MAGIC {
        return Err(KeystoreError::BadMagic);
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(&plaintext[..32]);
    plaintext.zeroize();

    let kp = Keypair::from_seed(&seed)
        .map_err(|e| KeystoreError::InvalidFormat(format!("keypair from seed: {e}")))?;
    if kp.pubkey().to_string() != ext.pubkey {
        seed.zeroize();
        return Err(KeystoreError::InvalidFormat(
            "decrypted secret does not match stored pubkey".into(),
        ));
    }
    let ks = encrypt(&seed, &ext.pubkey, password)?;
    seed.zeroize();
    Ok((ks, kp))
}

/// Load a keystore from disk + decrypt in one call.
pub fn load_from_path(path: &std::path::Path, password: &str) -> Result<Keypair, KeystoreError> {
    let bytes = std::fs::read(path)?;
    let ks: Keystore = serde_json::from_slice(&bytes)?;
    decrypt(&ks, password)
}

/// Save keystore JSON to disk with restrictive permissions on
/// Unix (0600). On Windows the OS handles ACL inheritance.
///
/// Creates the parent directory if missing — on first launch the Tauri
/// app's config dir (`~/Library/Application Support/com.degenbox.signer/`
/// on macOS, `%APPDATA%/com.degenbox.signer/` on Windows, XDG_CONFIG_HOME
/// on Linux) doesn't exist yet, and a bare `fs::write` fails with the
/// confusing `os error 2 — No such file or directory`.
pub fn save_to_path(ks: &Keystore, path: &std::path::Path) -> Result<(), KeystoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(ks)?;
    std::fs::write(path, &json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weak_argon2() -> Argon2Params {
        // Tests use weakened Argon2 params (1 MiB / 1 iter) so they
        // run in <100 ms instead of 200 ms+ each. The default
        // production params still go through `default()`.
        Argon2Params {
            m_cost: 1024,
            t_cost: 1,
            p_cost: 1,
        }
    }

    fn round_trip_with(params: Argon2Params) {
        let kp = Keypair::new();
        let pub_b58 = kp.pubkey().to_string();
        let secret = kp.secret_bytes();

        // Encrypt with the supplied params.
        let mut salt = [0u8; 32];
        OsRng.fill_bytes(&mut salt);
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let mut derived = [0u8; 32];
        derive_key("hunter2", &salt, &params, &mut derived).unwrap();
        let mut plaintext = [0u8; 32 + 4];
        plaintext[..32].copy_from_slice(secret.as_slice());
        plaintext[32..].copy_from_slice(MAGIC);
        let cipher = Aes256Gcm::new(&derived.into());
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_slice())
            .unwrap();
        let ks = Keystore {
            version: KEYSTORE_VERSION,
            kdf: "argon2id".into(),
            argon2: params,
            salt: hex::encode(salt),
            nonce: hex::encode(nonce_bytes),
            ciphertext: hex::encode(ciphertext),
            pubkey: pub_b58,
        };

        let kp2 = decrypt(&ks, "hunter2").unwrap();
        assert_eq!(kp2.pubkey(), kp.pubkey());
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        round_trip_with(weak_argon2());
    }

    #[test]
    fn wrong_password_fails() {
        let kp = Keypair::new();
        let secret = kp.secret_bytes();
        let ks = encrypt(secret, &kp.pubkey().to_string(), "right").unwrap();
        let r = decrypt(&ks, "wrong");
        assert!(matches!(r, Err(KeystoreError::BadPassword)));
    }

    #[test]
    fn corrupted_ciphertext_fails() {
        let kp = Keypair::new();
        let secret = kp.secret_bytes();
        let mut ks = encrypt(secret, &kp.pubkey().to_string(), "pw").unwrap();
        // Flip one byte in the ciphertext.
        let mut bytes = hex::decode(&ks.ciphertext).unwrap();
        bytes[0] ^= 0xff;
        ks.ciphertext = hex::encode(bytes);
        let r = decrypt(&ks, "pw");
        // AES-GCM tag should reject; we surface as BadPassword.
        assert!(matches!(r, Err(KeystoreError::BadPassword)));
    }

    #[test]
    fn unsupported_version_rejected() {
        let kp = Keypair::new();
        let secret = kp.secret_bytes();
        let mut ks = encrypt(secret, &kp.pubkey().to_string(), "pw").unwrap();
        ks.version = 99;
        let r = decrypt(&ks, "pw");
        assert!(matches!(r, Err(KeystoreError::UnsupportedVersion(99))));
    }

    #[test]
    fn pubkey_mismatch_rejected() {
        let kp = Keypair::new();
        let secret = kp.secret_bytes();
        let mut ks = encrypt(secret, &kp.pubkey().to_string(), "pw").unwrap();
        // Tamper the declared pubkey.
        ks.pubkey = "11111111111111111111111111111111".to_string();
        let r = decrypt(&ks, "pw");
        assert!(matches!(r, Err(KeystoreError::InvalidFormat(_))));
    }

    // ─── extension import ─────────────────────────────────────────────

    /// Build an extension-format keystore JSON for `kp` using the same
    /// crypto primitives (weak Argon2 params for test speed).
    fn extension_json_for(kp: &Keypair, password: &str) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let params = weak_argon2();
        let mut salt = [0u8; 32];
        OsRng.fill_bytes(&mut salt);
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let mut derived = [0u8; 32];
        derive_key(password, &salt, &params, &mut derived).unwrap();
        let mut plaintext = [0u8; 36];
        plaintext[..32].copy_from_slice(kp.secret_bytes().as_slice());
        plaintext[32..].copy_from_slice(MAGIC);
        let cipher = Aes256Gcm::new(&derived.into());
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_slice())
            .unwrap();
        serde_json::json!({
            "v": 1,
            "pubkey": kp.pubkey().to_string(),
            "kdf": "argon2id",
            "kdf_params": {
                "m": params.m_cost,
                "t": params.t_cost,
                "p": params.p_cost,
                "salt_b64": b64.encode(salt),
            },
            "cipher": "aes-256-gcm",
            "nonce_b64": b64.encode(nonce_bytes),
            "ct_b64": b64.encode(ct),
        })
        .to_string()
    }

    #[test]
    fn extension_import_round_trips_to_native_format() {
        let kp = Keypair::new();
        let json = extension_json_for(&kp, "hunter2");
        let (ks, imported) = import_extension_json(&json, "hunter2").unwrap();
        assert_eq!(imported.pubkey(), kp.pubkey());
        assert_eq!(ks.pubkey, kp.pubkey().to_string());
        // The re-encrypted native keystore decrypts with the same password.
        let kp2 = decrypt(&ks, "hunter2").unwrap();
        assert_eq!(kp2.pubkey(), kp.pubkey());
    }

    #[test]
    fn extension_import_rejects_wrong_password_and_garbage() {
        let kp = Keypair::new();
        let json = extension_json_for(&kp, "right");
        assert!(matches!(
            import_extension_json(&json, "wrong"),
            Err(KeystoreError::BadPassword)
        ));
        assert!(matches!(
            import_extension_json("{not json", "pw"),
            Err(KeystoreError::InvalidFormat(_))
        ));
        // Native Rust keystore JSON must be rejected as not-extension
        // (field names differ) instead of mis-importing.
        let native = encrypt(kp.secret_bytes(), &kp.pubkey().to_string(), "pw").unwrap();
        let native_json = serde_json::to_string(&native).unwrap();
        assert!(import_extension_json(&native_json, "pw").is_err());
    }
}
