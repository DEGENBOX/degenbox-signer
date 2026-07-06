//! Multi-wallet vault — N Solana + N Hyperliquid wallets under ONE
//! master password.
//!
//! ## Layout
//!
//! ```text
//! <default_dir>/vault/
//!   vault.json                    — manifest (metadata only, no keys)
//!   sol-<pubkey>.json             — per-wallet Solana keystore (crate::keystore format)
//!   hl-<address>.json             — per-wallet HL agent keystore (crate::hl::keystore format)
//!   hl-<address>.config.json      — per-wallet HL pairing config (HlConfig shape)
//!   hl-<address>.executed.jsonl   — per-wallet executed-marker ledger
//! ```
//!
//! The crypto is exactly the existing per-chain keystore crypto —
//! Argon2id + AES-256-GCM for Solana, the legacy-Go-compatible Argon2id
//! scheme for HL. The vault does NOT introduce a new envelope; it is a
//! manifest over N files all encrypted under the same master password.
//!
//! ## Master-password invariant
//!
//! `vault.json` carries a `verifier`: a throwaway Solana keystore whose
//! only purpose is to check a candidate password BEFORE a new wallet is
//! appended — so the vault can never end up with entries under mixed
//! passwords. (Pre-existing keystores migrated from the single-file
//! era were already unlocked with the same app password, and migration
//! verifies each decrypts before adopting it.)
//!
//! ## Migration
//!
//! [`Vault::migrate_legacy`] adopts the single-file era keystores
//! (`sol-keystore.json` / `hl-keystore.json` in the parent dir):
//! verify-decrypt under the master password, copy into the vault,
//! then rename the original to `<file>.bak` (non-destructive — the
//! ciphertext is preserved byte-for-byte in both places). The global
//! `hl-config.json` is COPIED to the migrated HL wallet's per-wallet
//! config and left in place (the CLI signer still reads it).

use crate::hl::keystore as hl_keystore;
use crate::keystore::{self, Keystore, KeystoreError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solana_sdk::signature::Keypair;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Manifest schema version.
pub const VAULT_VERSION: u8 = 1;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("keystore: {0}")]
    Keystore(#[from] KeystoreError),
    #[error("hl keystore: {0}")]
    HlKeystore(#[from] hl_keystore::HlKeystoreError),
    #[error("master password incorrect")]
    BadPassword,
    #[error("unsupported vault version {0} (this build supports {VAULT_VERSION})")]
    UnsupportedVersion(u8),
    #[error("wallet not found: {0}")]
    NotFound(String),
    #[error("wallet already in vault: {0}")]
    Duplicate(String),
    #[error("{0}")]
    Invalid(String),
}

/// Which chain a vault wallet signs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WalletChain {
    Sol,
    Hl,
}

impl WalletChain {
    pub fn as_str(&self) -> &'static str {
        match self {
            WalletChain::Sol => "sol",
            WalletChain::Hl => "hl",
        }
    }
}

/// One wallet ("client") in the vault. Metadata only — the encrypted
/// key material lives in `file` next to the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletEntry {
    /// Stable local id (uuid v4) — the IPC + UI "client id".
    pub id: String,
    pub chain: WalletChain,
    /// Base58 pubkey (sol) or 0x address (hl). Public.
    pub address: String,
    #[serde(default)]
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Keystore filename, relative to the vault dir.
    pub file: String,
    /// Local per-client pause flag (the per-client kill-switch; the
    /// global one stays in the host app).
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultMeta {
    pub version: u8,
    /// Password verifier: a throwaway Solana keystore encrypted under
    /// the master password. Never used for signing.
    pub verifier: Keystore,
    /// Designated primary wallet ids (the back-compat single-wallet
    /// surfaces bind to these). Default = first of chain.
    #[serde(default)]
    pub primary_sol: Option<String>,
    #[serde(default)]
    pub primary_hl: Option<String>,
    #[serde(default)]
    pub wallets: Vec<WalletEntry>,
    /// Forward-compatible keys newer builds may add.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Outcome of a legacy single-keystore migration.
#[derive(Debug, Default, Clone, Serialize)]
pub struct MigrationReport {
    pub migrated_sol: Option<String>,
    pub migrated_hl: Option<String>,
    /// Human-readable notes (e.g. a keystore that did NOT decrypt under
    /// the master password and was left untouched).
    pub notes: Vec<String>,
}

/// The on-disk vault: a directory + its parsed manifest.
#[derive(Debug, Clone)]
pub struct Vault {
    dir: PathBuf,
    pub meta: VaultMeta,
}

impl Vault {
    /// Does a vault manifest exist in `dir`?
    pub fn exists(dir: &Path) -> bool {
        dir.join("vault.json").is_file()
    }

    /// Create a fresh vault in `dir` under `password`. Errors if a
    /// manifest already exists (open it instead).
    pub fn create(dir: &Path, password: &str) -> Result<Self, VaultError> {
        if Self::exists(dir) {
            return Err(VaultError::Invalid(format!(
                "vault already exists at {}",
                dir.display()
            )));
        }
        fs::create_dir_all(dir)?;
        // Throwaway sentinel keypair — encrypted, then dropped. Only
        // its decryptability matters.
        let (verifier, kp) = keystore::generate(password)?;
        drop(kp);
        let meta = VaultMeta {
            version: VAULT_VERSION,
            verifier,
            primary_sol: None,
            primary_hl: None,
            wallets: Vec::new(),
            extra: serde_json::Map::new(),
        };
        let v = Self {
            dir: dir.to_path_buf(),
            meta,
        };
        v.save()?;
        Ok(v)
    }

    /// Open an existing vault (no password needed — listing wallets and
    /// addresses is public metadata).
    pub fn open(dir: &Path) -> Result<Self, VaultError> {
        let bytes = fs::read(dir.join("vault.json"))?;
        let meta: VaultMeta = serde_json::from_slice(&bytes)?;
        if meta.version != VAULT_VERSION {
            return Err(VaultError::UnsupportedVersion(meta.version));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            meta,
        })
    }

    /// Open if present, else create under `password`.
    pub fn open_or_create(dir: &Path, password: &str) -> Result<Self, VaultError> {
        if Self::exists(dir) {
            Self::open(dir)
        } else {
            Self::create(dir, password)
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Check a candidate master password against the verifier.
    pub fn verify_password(&self, password: &str) -> Result<(), VaultError> {
        match keystore::decrypt(&self.meta.verifier, password) {
            Ok(_) => Ok(()),
            Err(KeystoreError::BadPassword) => Err(VaultError::BadPassword),
            Err(e) => Err(e.into()),
        }
    }

    /// Atomic manifest write (tempfile + rename, 0600 on Unix).
    pub fn save(&self) -> Result<(), VaultError> {
        fs::create_dir_all(&self.dir)?;
        let bytes = serde_json::to_vec_pretty(&self.meta)?;
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        tmp.write_all(&bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o600));
        }
        tmp.persist(self.dir.join("vault.json"))
            .map_err(|e| VaultError::Io(e.error))?;
        Ok(())
    }

    pub fn wallets(&self) -> &[WalletEntry] {
        &self.meta.wallets
    }

    pub fn get(&self, id: &str) -> Option<&WalletEntry> {
        self.meta.wallets.iter().find(|w| w.id == id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut WalletEntry> {
        self.meta.wallets.iter_mut().find(|w| w.id == id)
    }

    /// The designated primary wallet for `chain` — explicit designation
    /// first, else the first wallet of that chain.
    pub fn primary(&self, chain: WalletChain) -> Option<&WalletEntry> {
        let designated = match chain {
            WalletChain::Sol => self.meta.primary_sol.as_deref(),
            WalletChain::Hl => self.meta.primary_hl.as_deref(),
        };
        designated
            .and_then(|id| self.get(id))
            .filter(|w| w.chain == chain)
            .or_else(|| self.meta.wallets.iter().find(|w| w.chain == chain))
    }

    /// Designate `id` as its chain's primary. Persists.
    pub fn set_primary(&mut self, id: &str) -> Result<(), VaultError> {
        let chain = self
            .get(id)
            .ok_or_else(|| VaultError::NotFound(id.to_string()))?
            .chain;
        match chain {
            WalletChain::Sol => self.meta.primary_sol = Some(id.to_string()),
            WalletChain::Hl => self.meta.primary_hl = Some(id.to_string()),
        }
        self.save()
    }

    pub fn set_label(&mut self, id: &str, label: Option<String>) -> Result<(), VaultError> {
        let w = self
            .get_mut(id)
            .ok_or_else(|| VaultError::NotFound(id.to_string()))?;
        w.label = label.filter(|l| !l.trim().is_empty());
        self.save()
    }

    pub fn set_paused(&mut self, id: &str, paused: bool) -> Result<(), VaultError> {
        let w = self
            .get_mut(id)
            .ok_or_else(|| VaultError::NotFound(id.to_string()))?;
        w.paused = paused;
        self.save()
    }

    fn assert_new_address(&self, chain: WalletChain, address: &str) -> Result<(), VaultError> {
        if self
            .meta
            .wallets
            .iter()
            .any(|w| w.chain == chain && w.address.eq_ignore_ascii_case(address))
        {
            return Err(VaultError::Duplicate(address.to_string()));
        }
        Ok(())
    }

    fn push_entry(
        &mut self,
        chain: WalletChain,
        address: String,
        label: Option<String>,
        file: String,
    ) -> Result<WalletEntry, VaultError> {
        let entry = WalletEntry {
            id: uuid::Uuid::new_v4().to_string(),
            chain,
            address,
            label: label.filter(|l| !l.trim().is_empty()),
            created_at: Utc::now(),
            file,
            paused: false,
        };
        self.meta.wallets.push(entry.clone());
        self.save()?;
        Ok(entry)
    }

    // ── add / import ────────────────────────────────────────────────

    /// Append a Solana wallet from its 32-byte seed. Verifies the
    /// master password against the verifier first so a typo can never
    /// split the vault across two passwords.
    pub fn add_sol(
        &mut self,
        seed: &[u8; 32],
        password: &str,
        label: Option<String>,
    ) -> Result<WalletEntry, VaultError> {
        self.verify_password(password)?;
        use solana_sdk::signature::{SeedDerivable, Signer as _};
        let kp = Keypair::from_seed(seed)
            .map_err(|e| VaultError::Invalid(format!("keypair from seed: {e}")))?;
        let pubkey = kp.pubkey().to_string();
        drop(kp);
        self.assert_new_address(WalletChain::Sol, &pubkey)?;
        let ks = keystore::encrypt(seed, &pubkey, password)?;
        let file = format!("sol-{pubkey}.json");
        keystore::save_to_path(&ks, &self.dir.join(&file))?;
        self.push_entry(WalletChain::Sol, pubkey, label, file)
    }

    /// Append an already-encrypted Solana keystore (file import / CLI
    /// migration). The blob must decrypt under the MASTER password —
    /// rejected otherwise so the one-password invariant holds.
    pub fn adopt_sol_keystore(
        &mut self,
        ks: &Keystore,
        password: &str,
        label: Option<String>,
    ) -> Result<WalletEntry, VaultError> {
        let kp = match keystore::decrypt(ks, password) {
            Ok(kp) => kp,
            Err(KeystoreError::BadPassword) => return Err(VaultError::BadPassword),
            Err(e) => return Err(e.into()),
        };
        drop(kp);
        self.assert_new_address(WalletChain::Sol, &ks.pubkey)?;
        let file = format!("sol-{}.json", ks.pubkey);
        keystore::save_to_path(ks, &self.dir.join(&file))?;
        self.push_entry(WalletChain::Sol, ks.pubkey.clone(), label, file)
    }

    /// Append an HL agent wallet from its 32-byte secp256k1 secret
    /// (hex, 0x-prefix optional).
    pub fn add_hl(
        &mut self,
        private_key_hex: &str,
        password: &str,
        label: Option<String>,
    ) -> Result<WalletEntry, VaultError> {
        self.verify_password(password)?;
        // Derive the address first so the duplicate check runs before
        // any file is written.
        let key_bytes = hex::decode(private_key_hex.trim().trim_start_matches("0x"))
            .map_err(|e| VaultError::Invalid(format!("private key hex: {e}")))?;
        if key_bytes.len() != 32 {
            return Err(VaultError::Invalid(
                "private key must be 32 bytes hex".into(),
            ));
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&key_bytes);
        let address = hl_keystore::derive_address(&secret)?;
        {
            use zeroize::Zeroize as _;
            secret.zeroize();
        }
        self.assert_new_address(WalletChain::Hl, &address)?;
        let file = format!("hl-{address}.json");
        hl_keystore::save(private_key_hex, password.as_bytes(), &self.dir.join(&file))?;
        self.push_entry(WalletChain::Hl, address, label, file)
    }

    // ── unlock ──────────────────────────────────────────────────────

    /// Decrypt one Solana wallet.
    pub fn unlock_sol(&self, id: &str, password: &str) -> Result<Keypair, VaultError> {
        let w = self
            .get(id)
            .ok_or_else(|| VaultError::NotFound(id.to_string()))?;
        if w.chain != WalletChain::Sol {
            return Err(VaultError::Invalid(format!("{id} is not a Solana wallet")));
        }
        match keystore::load_from_path(&self.dir.join(&w.file), password) {
            Ok(kp) => Ok(kp),
            Err(KeystoreError::BadPassword) => Err(VaultError::BadPassword),
            Err(e) => Err(e.into()),
        }
    }

    /// Decrypt one HL wallet → `(secret_hex, address)`.
    pub fn unlock_hl(&self, id: &str, password: &str) -> Result<(String, String), VaultError> {
        let w = self
            .get(id)
            .ok_or_else(|| VaultError::NotFound(id.to_string()))?;
        if w.chain != WalletChain::Hl {
            return Err(VaultError::Invalid(format!("{id} is not an HL wallet")));
        }
        match hl_keystore::load(&self.dir.join(&w.file), password.as_bytes()) {
            Ok(pair) => Ok(pair),
            Err(hl_keystore::HlKeystoreError::BadPassphrase) => Err(VaultError::BadPassword),
            Err(e) => Err(e.into()),
        }
    }

    // ── remove / export ─────────────────────────────────────────────

    /// Remove a wallet from the vault. Non-destructive on disk: the
    /// keystore file is renamed to `<file>.removed.bak` so an
    /// accidental removal never destroys ciphertext. The host UI gates
    /// this behind an explicit confirmation.
    pub fn remove(&mut self, id: &str) -> Result<WalletEntry, VaultError> {
        let idx = self
            .meta
            .wallets
            .iter()
            .position(|w| w.id == id)
            .ok_or_else(|| VaultError::NotFound(id.to_string()))?;
        let entry = self.meta.wallets.remove(idx);
        if self.meta.primary_sol.as_deref() == Some(id) {
            self.meta.primary_sol = None;
        }
        if self.meta.primary_hl.as_deref() == Some(id) {
            self.meta.primary_hl = None;
        }
        let src = self.dir.join(&entry.file);
        if src.exists() {
            let bak = self.dir.join(format!("{}.removed.bak", entry.file));
            fs::rename(&src, &bak)?;
        }
        self.save()?;
        Ok(entry)
    }

    /// The raw encrypted keystore JSON for one wallet (for export —
    /// what leaves the app is the same envelope that sits on disk,
    /// never plaintext key material).
    pub fn export_keystore_json(&self, id: &str) -> Result<String, VaultError> {
        let w = self
            .get(id)
            .ok_or_else(|| VaultError::NotFound(id.to_string()))?;
        Ok(fs::read_to_string(self.dir.join(&w.file))?)
    }

    /// Absolute path of a wallet's keystore file.
    pub fn keystore_path(&self, entry: &WalletEntry) -> PathBuf {
        self.dir.join(&entry.file)
    }

    // ── per-wallet HL side-files ────────────────────────────────────

    /// Per-wallet HL pairing config path (HlConfig JSON shape).
    pub fn hl_config_path(&self, entry: &WalletEntry) -> PathBuf {
        self.dir.join(format!("hl-{}.config.json", entry.address))
    }

    /// Per-wallet executed-marker ledger path — two HL daemons must
    /// NEVER share one idempotency ledger (same invariant as the CLI
    /// hub's per-bot `executed.jsonl`).
    pub fn hl_executed_path(&self, entry: &WalletEntry) -> PathBuf {
        self.dir
            .join(format!("hl-{}.executed.jsonl", entry.address))
    }

    // ── legacy migration ────────────────────────────────────────────

    /// Adopt the single-file era keystores into the vault. For each
    /// existing legacy file that decrypts under `password`: copy into
    /// the vault, then rename the original to `<file>.bak`. Files that
    /// do NOT decrypt (different password) are left untouched and
    /// reported in `notes`. Idempotent: already-adopted addresses are
    /// skipped.
    pub fn migrate_legacy(
        &mut self,
        sol_keystore: &Path,
        hl_keystore_path: &Path,
        hl_config: Option<&Path>,
        password: &str,
    ) -> Result<MigrationReport, VaultError> {
        let mut report = MigrationReport::default();

        if sol_keystore.is_file() {
            let bytes = fs::read(sol_keystore)?;
            match serde_json::from_slice::<Keystore>(&bytes) {
                Ok(ks) => match self.adopt_sol_keystore(&ks, password, None) {
                    Ok(entry) => {
                        fs::rename(sol_keystore, sol_keystore.with_extension("json.bak"))?;
                        report.migrated_sol = Some(entry.address.clone());
                    }
                    Err(VaultError::Duplicate(addr)) => {
                        report
                            .notes
                            .push(format!("legacy Solana keystore {addr} already in vault"));
                    }
                    Err(VaultError::BadPassword) => {
                        report.notes.push(
                            "legacy Solana keystore does not decrypt under the master password — left in place"
                                .into(),
                        );
                    }
                    Err(e) => return Err(e),
                },
                Err(e) => {
                    report
                        .notes
                        .push(format!("legacy Solana keystore unreadable: {e}"));
                }
            }
        }

        if hl_keystore_path.is_file() {
            match hl_keystore::load(hl_keystore_path, password.as_bytes()) {
                Ok((_secret_hex, address)) => {
                    if self.assert_new_address(WalletChain::Hl, &address).is_err() {
                        report
                            .notes
                            .push(format!("legacy HL keystore {address} already in vault"));
                    } else {
                        // Byte-for-byte copy — the envelope already
                        // decrypts under the master password.
                        let file = format!("hl-{address}.json");
                        fs::copy(hl_keystore_path, self.dir.join(&file))?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = fs::set_permissions(
                                self.dir.join(&file),
                                fs::Permissions::from_mode(0o600),
                            );
                        }
                        let entry =
                            self.push_entry(WalletChain::Hl, address.clone(), None, file)?;
                        // Adopt the global pairing config as this
                        // wallet's per-wallet config; the global file
                        // stays for CLI interop.
                        if let Some(cfg) = hl_config {
                            if cfg.is_file() {
                                let _ = fs::copy(cfg, self.hl_config_path(&entry));
                            }
                        }
                        fs::rename(
                            hl_keystore_path,
                            hl_keystore_path.with_extension("json.bak"),
                        )?;
                        report.migrated_hl = Some(address);
                    }
                }
                Err(hl_keystore::HlKeystoreError::BadPassphrase) => {
                    report.notes.push(
                        "legacy HL keystore does not decrypt under the master password — left in place"
                            .into(),
                    );
                }
                Err(e) => {
                    report
                        .notes
                        .push(format!("legacy HL keystore unreadable: {e}"));
                }
            }
        }

        Ok(report)
    }
}

/// Default vault dir: `<default_dir>/vault`.
pub fn default_vault_dir() -> Result<PathBuf, crate::paths::PathsError> {
    Ok(crate::paths::default_dir()?.join("vault"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::{Keypair as SolKeypair, Signer as _};
    use tempfile::tempdir;

    const HL_KEY_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const HL_KEY_B: &str = "00000000000000000000000000000000000000000000000000000000000000aa";

    fn seed_of(kp: &SolKeypair) -> [u8; 32] {
        let mut s = [0u8; 32];
        s.copy_from_slice(kp.secret_bytes().as_slice());
        s
    }

    #[test]
    fn create_verify_and_reject_wrong_password() {
        let dir = tempdir().unwrap();
        let v = Vault::create(dir.path(), "hunter2").unwrap();
        v.verify_password("hunter2").unwrap();
        assert!(matches!(
            v.verify_password("wrong"),
            Err(VaultError::BadPassword)
        ));
        // Re-open without password keeps metadata readable.
        let v2 = Vault::open(dir.path()).unwrap();
        assert!(v2.wallets().is_empty());
    }

    #[test]
    fn multi_wallet_add_unlock_and_primary_designation() {
        let dir = tempdir().unwrap();
        let mut v = Vault::create(dir.path(), "pw").unwrap();

        let kp1 = SolKeypair::new();
        let kp2 = SolKeypair::new();
        let s1 = v
            .add_sol(&seed_of(&kp1), "pw", Some("main".into()))
            .unwrap();
        let s2 = v.add_sol(&seed_of(&kp2), "pw", None).unwrap();
        let h1 = v.add_hl(HL_KEY_A, "pw", Some("hl one".into())).unwrap();
        let h2 = v.add_hl(HL_KEY_B, "pw", None).unwrap();
        assert_eq!(v.wallets().len(), 4);

        // Default primary = first of chain.
        assert_eq!(v.primary(WalletChain::Sol).unwrap().id, s1.id);
        assert_eq!(v.primary(WalletChain::Hl).unwrap().id, h1.id);
        // Explicit designation persists across re-open.
        v.set_primary(&s2.id).unwrap();
        let v2 = Vault::open(dir.path()).unwrap();
        assert_eq!(v2.primary(WalletChain::Sol).unwrap().id, s2.id);

        // Every wallet unlocks independently under the ONE password.
        let u1 = v.unlock_sol(&s1.id, "pw").unwrap();
        assert_eq!(u1.pubkey(), kp1.pubkey());
        let u2 = v.unlock_sol(&s2.id, "pw").unwrap();
        assert_eq!(u2.pubkey(), kp2.pubkey());
        let (sec, addr) = v.unlock_hl(&h1.id, "pw").unwrap();
        assert_eq!(sec, HL_KEY_A);
        assert_eq!(addr, h1.address);
        let (_, addr2) = v.unlock_hl(&h2.id, "pw").unwrap();
        assert_eq!(addr2, h2.address);

        // Wrong password fails per wallet.
        assert!(matches!(
            v.unlock_sol(&s1.id, "nope"),
            Err(VaultError::BadPassword)
        ));
        assert!(matches!(
            v.unlock_hl(&h1.id, "nope"),
            Err(VaultError::BadPassword)
        ));
    }

    #[test]
    fn add_rejects_wrong_master_password_and_duplicates() {
        let dir = tempdir().unwrap();
        let mut v = Vault::create(dir.path(), "pw").unwrap();
        let kp = SolKeypair::new();
        assert!(matches!(
            v.add_sol(&seed_of(&kp), "other", None),
            Err(VaultError::BadPassword)
        ));
        v.add_sol(&seed_of(&kp), "pw", None).unwrap();
        assert!(matches!(
            v.add_sol(&seed_of(&kp), "pw", None),
            Err(VaultError::Duplicate(_))
        ));
        v.add_hl(HL_KEY_A, "pw", None).unwrap();
        assert!(matches!(
            v.add_hl(HL_KEY_A, "pw", None),
            Err(VaultError::Duplicate(_))
        ));
    }

    #[test]
    fn per_wallet_hl_side_paths_are_distinct() {
        let dir = tempdir().unwrap();
        let mut v = Vault::create(dir.path(), "pw").unwrap();
        let h1 = v.add_hl(HL_KEY_A, "pw", None).unwrap();
        let h2 = v.add_hl(HL_KEY_B, "pw", None).unwrap();
        assert_ne!(v.hl_config_path(&h1), v.hl_config_path(&h2));
        assert_ne!(v.hl_executed_path(&h1), v.hl_executed_path(&h2));
        assert_ne!(v.keystore_path(&h1), v.keystore_path(&h2));
    }

    #[test]
    fn remove_is_non_destructive_and_export_round_trips() {
        let dir = tempdir().unwrap();
        let mut v = Vault::create(dir.path(), "pw").unwrap();
        let kp = SolKeypair::new();
        let e = v.add_sol(&seed_of(&kp), "pw", None).unwrap();

        // Export is the on-disk envelope verbatim.
        let json = v.export_keystore_json(&e.id).unwrap();
        let ks: Keystore = serde_json::from_str(&json).unwrap();
        assert_eq!(ks.pubkey, e.address);

        let removed = v.remove(&e.id).unwrap();
        assert!(v.wallets().is_empty());
        assert!(!v.keystore_path(&removed).exists());
        assert!(dir
            .path()
            .join(format!("{}.removed.bak", removed.file))
            .exists());
    }

    #[test]
    fn migration_adopts_legacy_keystores_non_destructively() {
        let dir = tempdir().unwrap();
        let legacy_dir = dir.path().join("legacy");
        fs::create_dir_all(&legacy_dir).unwrap();
        // Legacy single keystores, both under the SAME app password
        // (the pre-vault app unlocked both with one password).
        let kp = SolKeypair::new();
        let (ks, _) = {
            let seed = seed_of(&kp);
            let ks = keystore::encrypt(&seed, &kp.pubkey().to_string(), "pw").unwrap();
            (ks, ())
        };
        let sol_path = legacy_dir.join("sol-keystore.json");
        keystore::save_to_path(&ks, &sol_path).unwrap();
        let hl_path = legacy_dir.join("hl-keystore.json");
        let hl_addr = hl_keystore::save(HL_KEY_A, b"pw", &hl_path).unwrap();
        let cfg_path = legacy_dir.join("hl-config.json");
        fs::write(
            &cfg_path,
            br#"{"server_url":"https://x","api_token":"tok"}"#,
        )
        .unwrap();

        let vault_dir = dir.path().join("vault");
        let mut v = Vault::create(&vault_dir, "pw").unwrap();
        let report = v
            .migrate_legacy(&sol_path, &hl_path, Some(&cfg_path), "pw")
            .unwrap();
        assert_eq!(report.migrated_sol.as_deref(), Some(ks.pubkey.as_str()));
        assert_eq!(report.migrated_hl.as_deref(), Some(hl_addr.as_str()));
        assert_eq!(v.wallets().len(), 2);

        // Originals renamed to .bak (ciphertext preserved), vault
        // copies decrypt, per-wallet HL config adopted.
        assert!(!sol_path.exists());
        assert!(sol_path.with_extension("json.bak").exists());
        assert!(!hl_path.exists());
        assert!(hl_path.with_extension("json.bak").exists());
        assert!(
            cfg_path.exists(),
            "global hl-config must stay (CLI interop)"
        );
        let sol_entry = v
            .wallets()
            .iter()
            .find(|w| w.chain == WalletChain::Sol)
            .unwrap()
            .clone();
        let hl_entry = v
            .wallets()
            .iter()
            .find(|w| w.chain == WalletChain::Hl)
            .unwrap()
            .clone();
        let unlocked = v.unlock_sol(&sol_entry.id, "pw").unwrap();
        assert_eq!(unlocked.pubkey(), kp.pubkey());
        let (sec, addr) = v.unlock_hl(&hl_entry.id, "pw").unwrap();
        assert_eq!(sec, HL_KEY_A);
        assert_eq!(addr, hl_addr);
        assert!(v.hl_config_path(&hl_entry).exists());

        // Idempotent: a second migration call is a no-op (files gone).
        let report2 = v
            .migrate_legacy(&sol_path, &hl_path, Some(&cfg_path), "pw")
            .unwrap();
        assert!(report2.migrated_sol.is_none() && report2.migrated_hl.is_none());
        assert_eq!(v.wallets().len(), 2);
    }

    #[test]
    fn migration_leaves_undecryptable_legacy_files_untouched() {
        let dir = tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        fs::create_dir_all(&legacy).unwrap();
        let kp = SolKeypair::new();
        let seed = seed_of(&kp);
        let ks = keystore::encrypt(&seed, &kp.pubkey().to_string(), "DIFFERENT").unwrap();
        let sol_path = legacy.join("sol-keystore.json");
        keystore::save_to_path(&ks, &sol_path).unwrap();

        let mut v = Vault::create(&dir.path().join("vault"), "pw").unwrap();
        let report = v
            .migrate_legacy(&sol_path, &legacy.join("missing-hl.json"), None, "pw")
            .unwrap();
        assert!(report.migrated_sol.is_none());
        assert!(!report.notes.is_empty());
        assert!(sol_path.exists(), "undecryptable original must survive");
        assert!(v.wallets().is_empty());
    }

    #[test]
    fn labels_and_pause_flags_persist() {
        let dir = tempdir().unwrap();
        let mut v = Vault::create(dir.path(), "pw").unwrap();
        let e = v.add_hl(HL_KEY_A, "pw", None).unwrap();
        v.set_label(&e.id, Some("scalper".into())).unwrap();
        v.set_paused(&e.id, true).unwrap();
        let v2 = Vault::open(dir.path()).unwrap();
        let w = v2.get(&e.id).unwrap();
        assert_eq!(w.label.as_deref(), Some("scalper"));
        assert!(w.paused);
    }
}
