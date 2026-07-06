//! Manifest-based self-update with ed25519 signature verification.
//!
//! This module is shared by the HL signer (`hl-signer-desktop`) and
//! the future Solana signer-desktop. The wire shape is the
//! `LatestManifest` produced by the gateway's `GET /signer/latest`
//! endpoint:
//!
//! ```json
//! {
//!   "version": "0.2.0",
//!   "platforms": {
//!     "aarch64-apple-darwin": {
//!       "url":         "https://…/hl-signer-desktop-aarch64-apple-darwin.tar.gz",
//!       "sha256":      "abc123…",
//!       "ed25519_sig": "deadbeef… (hex, 64 bytes)"
//!     },
//!     …
//!   }
//! }
//! ```
//!
//! The ed25519 signature is taken over the lowercase hex sha256 string
//! (`sha256.as_bytes()`). The verifying public key is baked into the
//! binary at build time via the `SIGNER_UPDATE_PUBKEY_HEX` env (or
//! supplied as an argument for tests).
//!
//! Crypto note: we don't add ed25519-dalek to signer-core's dependency
//! list to keep the wasm build slim. Instead, callers (the desktop
//! crates that already pull ed25519-dalek for HL signing) pass a
//! verifier closure.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformAsset {
    pub url: String,
    pub sha256: String,
    pub ed25519_sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestManifest {
    pub version: String,
    pub platforms: HashMap<String, PlatformAsset>,
}

/// Verifier closure: returns `Ok(())` when the signature is valid.
pub type VerifyFn = dyn Fn(&[u8] /* msg */, &[u8] /* sig */) -> Result<(), String>;

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("no asset for target {0}")]
    UnknownTarget(String),
    #[error("sha256 mismatch: expected {0}, got {1}")]
    Sha256(String, String),
    #[error("ed25519 verify: {0}")]
    Signature(String),
}

impl UpdateError {
    /// Shorthand for the `Sha256` variant — uses named fields in the
    /// `Display` impl above.
    pub fn sha256(expected: String, got: String) -> Self {
        Self::Sha256(expected, got)
    }
}

/// Verify a downloaded binary against its manifest entry. Both the
/// sha256 must match AND the ed25519 signature over `sha256.as_bytes()`
/// must validate against the verifier. Returns `Ok(())` only when both
/// checks pass.
pub fn verify_download(
    asset: &PlatformAsset,
    bytes: &[u8],
    verify: &VerifyFn,
) -> Result<(), UpdateError> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let got = hex::encode(h.finalize());
    if !got.eq_ignore_ascii_case(&asset.sha256) {
        return Err(UpdateError::sha256(asset.sha256.clone(), got));
    }
    let sig =
        hex::decode(&asset.ed25519_sig).map_err(|e| UpdateError::Signature(format!("hex: {e}")))?;
    verify(asset.sha256.as_bytes(), &sig).map_err(UpdateError::Signature)?;
    Ok(())
}

/// Determine the platform key (e.g. `aarch64-apple-darwin`) for the
/// currently-running binary. Returns `None` for unknown targets so the
/// caller can degrade gracefully.
pub fn current_target() -> Option<&'static str> {
    Some(match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        _ => return None,
    })
}

/// True when `latest` is strictly newer than `current` under
/// dot-separated numeric comparison. Treats non-numeric suffixes as 0.
pub fn is_newer(current: &str, latest: &str) -> bool {
    let c = parse_version(current);
    let l = parse_version(latest);
    l > c
}

fn parse_version(s: &str) -> (u32, u32, u32) {
    let mut it = s.trim_start_matches('v').split('.');
    let a = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let b = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let c = it
        .next()
        .and_then(|x| x.split('-').next().and_then(|y| y.parse().ok()))
        .unwrap_or(0);
    (a, b, c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_mismatch_rejected() {
        let asset = PlatformAsset {
            url: "x".into(),
            sha256: "00".repeat(32),
            ed25519_sig: "00".repeat(64),
        };
        let verify: Box<VerifyFn> = Box::new(|_m, _s| Ok(()));
        let err = verify_download(&asset, b"not-zeros", &*verify).unwrap_err();
        assert!(matches!(err, UpdateError::Sha256(_, _)));
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.1.0-rc1", "0.2.0"));
    }
}
