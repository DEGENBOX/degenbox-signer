//! Self-update for `hl-signer-desktop`.
//!
//! Mirrors the v1 Go bot's release flow: probe the GitHub Releases
//! API for the latest `v*-signer` tag, compare to our compiled-in
//! version, optionally download + verify (sha256) + replace the
//! current binary. Same pattern as the Solana sibling
//! (`degenbox-signer-cli`).
//!
//! - [`check`]            — one-shot version probe; returns the latest
//!                           tag if newer than us. Used at daemon
//!                           startup and on a 24h ticker.
//! - [`run_self_update`]  — invoked by the `self-update` subcommand.
//!                           Downloads the platform asset, validates
//!                           its sha256 against the release's
//!                           `SHASUMS256.txt`, atomically swaps the
//!                           current executable.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Repository that hosts the releases. Override via the
/// `DEGENBOX_RELEASES_REPO` env var so a fork or staging account can
/// point this at their own release stream during a rollout.
const DEFAULT_REPO: &str = "DEGENBOX/degenbox-v2";

/// Tag suffix this binary watches for. The HL signer ships under the
/// same `-signer` cadence as the Solana CLI.
const TAG_SUFFIX: &str = "-signer";

/// File name of the binary inside the release archive, and the
/// `<basename>-<tag>-<target>.tar.gz` prefix used for asset names.
const ASSET_BASENAME: &str = "hl-signer-desktop";

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub tag: String,
    pub html_url: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseResp {
    tag_name: String,
    html_url: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

fn repo() -> String {
    std::env::var("DEGENBOX_RELEASES_REPO").unwrap_or_else(|_| DEFAULT_REPO.to_string())
}

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// One-shot probe — returns `Some(UpdateInfo)` when a newer tag is
/// available. Never panics, never blocks longer than 10s; failures
/// downgrade to `None` so a flaky network never crashes the daemon.
pub async fn check() -> Option<UpdateInfo> {
    match check_inner().await {
        Ok(info) => info,
        Err(e) => {
            tracing::debug!("self-update check failed: {e:#}");
            None
        }
    }
}

async fn check_inner() -> Result<Option<UpdateInfo>> {
    let release = fetch_latest_release().await?;
    let tag = release.tag_name.clone();
    let Some(ver) = parse_tag(&tag) else {
        return Ok(None);
    };
    let current = current_version();
    if is_newer(&ver, current) {
        Ok(Some(UpdateInfo {
            current: current.to_string(),
            latest: ver,
            tag,
            html_url: release.html_url,
        }))
    } else {
        Ok(None)
    }
}

/// Spawn a background ticker that re-checks every 24h and prints a
/// one-liner when an update lands. Safe to call multiple times — each
/// call gets its own task. Detached on purpose; we don't ever want a
/// check failure to take the daemon down with it.
pub fn spawn_daily_check() {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60 * 60 * 24));
        // First tick fires immediately — skip it so we don't double-
        // probe on startup. Callers run `check()` once on startup
        // themselves to print the initial notice if any.
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Some(info) = check().await {
                eprintln!();
                eprintln!(
                    "  update available: {} → {} ({}). Run `{} self-update` to upgrade.",
                    info.current, info.latest, info.tag, ASSET_BASENAME
                );
            }
        }
    });
}

/// `self-update` subcommand entrypoint.
pub async fn run_self_update() -> Result<()> {
    let release = fetch_latest_release().await?;
    let Some(ver) = parse_tag(&release.tag_name) else {
        return Err(anyhow!(
            "latest release tag `{}` does not look like v<ver>{TAG_SUFFIX}",
            release.tag_name
        ));
    };
    let current = current_version();
    if !is_newer(&ver, current) {
        println!("  already up to date ({current}).");
        return Ok(());
    }

    let target = current_target_triple()
        .ok_or_else(|| anyhow!("no prebuilt asset for this platform — build from source"))?;
    let archive_name = format!(
        "{ASSET_BASENAME}-{tag}-{target}.tar.gz",
        tag = release.tag_name
    );
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == archive_name)
        .ok_or_else(|| {
            anyhow!(
                "release {} has no asset named `{}` — possibly still building",
                release.tag_name,
                archive_name
            )
        })?;

    println!("  downloading {} …", asset.name);
    let bytes = download_bytes(&asset.browser_download_url).await?;

    // Validate against SHASUMS256.txt before touching disk.
    let shasums = release
        .assets
        .iter()
        .find(|a| a.name == "SHASUMS256.txt")
        .ok_or_else(|| anyhow!("release is missing SHASUMS256.txt — refusing to install"))?;
    let shasums_body = download_text(&shasums.browser_download_url).await?;
    let expected = parse_shasum(&shasums_body, &archive_name)
        .ok_or_else(|| anyhow!("no sha256 line for `{}` in SHASUMS256.txt", archive_name))?;
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(&expected) {
        return Err(anyhow!(
            "sha256 mismatch: expected {} got {}",
            expected,
            actual
        ));
    }
    println!("  sha256 verified ({})", &actual[..16]);

    let staged = stage_new_binary(&bytes, ASSET_BASENAME)?;
    let current_exe = std::env::current_exe().context("locate current executable")?;
    atomic_swap(&staged, &current_exe)?;
    println!("  upgraded to {} — restart the daemon.", release.tag_name);
    Ok(())
}

// ─── helpers ─────────────────────────────────────────────────────

async fn fetch_latest_release() -> Result<ReleaseResp> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let client = reqwest::Client::builder()
        .user_agent(concat!("hl-signer-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()?;
    let release: ReleaseResp = resp.json().await.context("decode release JSON")?;
    Ok(release)
}

async fn download_bytes(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("hl-signer-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
}

async fn download_text(url: &str) -> Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("hl-signer-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15))
        .build()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.text().await?)
}

/// Strip the `v` prefix and `-signer` suffix off a release tag and
/// return the inner version string. Returns `None` for tags that
/// don't belong to this binary's release stream.
fn parse_tag(tag: &str) -> Option<String> {
    let trimmed = tag.strip_prefix('v')?.strip_suffix(TAG_SUFFIX)?;
    Some(trimmed.to_string())
}

/// Order-aware version comparison. We deliberately handle only the
/// MAJOR.MINOR.PATCH(-PRE) shape the release workflow validates;
/// anything else returns `false` (treat as "not newer") so a malformed
/// tag never trips a forced upgrade.
fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |s: &str| -> Option<(u32, u32, u32, Option<String>)> {
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p.to_string())),
            None => (s, None),
        };
        let mut it = core.split('.');
        let major = it.next()?.parse().ok()?;
        let minor = it.next()?.parse().ok()?;
        let patch = it.next()?.parse().ok()?;
        if it.next().is_some() {
            return None;
        }
        Some((major, minor, patch, pre))
    };
    let Some((cm, cn, cp, cpre)) = parse(candidate) else {
        return false;
    };
    let Some((rm, rn, rp, rpre)) = parse(current) else {
        return false;
    };
    if (cm, cn, cp) != (rm, rn, rp) {
        return (cm, cn, cp) > (rm, rn, rp);
    }
    // Same core version — releases without a pre-release suffix
    // outrank pre-releases (semver convention).
    match (cpre, rpre) {
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (Some(a), Some(b)) => a > b,
        (None, None) => false,
    }
}

fn current_target_triple() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else {
        None
    }
}

fn parse_shasum(body: &str, target_file: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `sha256sum` / `shasum -a 256` both emit `<hex>  <name>` (two
        // spaces). Some implementations use a single space; tolerate
        // either.
        let (sum, name) = line.split_once("  ").or_else(|| line.split_once(' '))?;
        let name = name.trim_start_matches('*').trim();
        if name == target_file {
            return Some(sum.trim().to_string());
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{:02x}", b);
    }
    out
}

/// Unpack the new binary out of the gzipped tarball into a temp dir
/// living for the lifetime of the install. The archive layout is the
/// one written by `release-signers.yml`:
/// `<basename>-<tag>-<target>/<basename>`.
fn stage_new_binary(archive: &[u8], binary_name: &str) -> Result<PathBuf> {
    let tmp_dir = tempfile::tempdir().context("mkdir tempdir")?;
    let dest = tmp_dir.path().to_owned();
    let gz = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(gz);
    tar.unpack(&dest).context("untar release archive")?;

    let mut found = None;
    for entry in std::fs::read_dir(&dest)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join(binary_name);
        if candidate.exists() {
            found = Some(candidate);
            break;
        }
    }
    let extracted = found.ok_or_else(|| {
        anyhow!("archive layout unexpected — couldn't find `{binary_name}` inside")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o755))?;
    }
    // Leak the tempdir guard so the staged file survives until the
    // process exits — atomic_swap moves it out, and we'd rather
    // leak a directory than risk Drop removing the binary before the
    // rename lands.
    let _ = Box::leak(Box::new(tmp_dir));
    Ok(extracted)
}

fn atomic_swap(new_binary: &Path, current_exe: &Path) -> Result<()> {
    let backup = current_exe.with_extension("bak");
    if backup.exists() {
        let _ = std::fs::remove_file(&backup);
    }
    // Some platforms (macOS) won't let us overwrite the running
    // binary directly. Move the running file aside first, then
    // rename the new one into place.
    std::fs::rename(current_exe, &backup).context("rename current exe → .bak")?;
    if let Err(e) = std::fs::rename(new_binary, current_exe) {
        // Best-effort rollback so we don't strand the user with no
        // binary at all.
        let _ = std::fs::rename(&backup, current_exe);
        return Err(anyhow!("install new binary: {e}"));
    }
    let _ = std::fs::remove_file(&backup);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tag_round_trip() {
        assert_eq!(parse_tag("v0.1.0-signer"), Some("0.1.0".into()));
        assert_eq!(parse_tag("v1.2.3-rc.1-signer"), Some("1.2.3-rc.1".into()));
        assert_eq!(parse_tag("v0.1.0-extension"), None);
        assert_eq!(parse_tag("nope"), None);
    }

    #[test]
    fn is_newer_basic() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.1"));
    }

    #[test]
    fn is_newer_prerelease() {
        // Plain release outranks an rc on the same core version.
        assert!(is_newer("0.1.0", "0.1.0-rc.1"));
        assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
        // Higher rc wins.
        assert!(is_newer("0.1.0-rc.2", "0.1.0-rc.1"));
    }

    #[test]
    fn parse_shasum_picks_right_line() {
        let body = "\
abc123  some-other.tar.gz
deadbeefcafe  hl-signer-desktop-v0.1.0-signer-aarch64-apple-darwin.tar.gz
9999  more.tar.gz
";
        assert_eq!(
            parse_shasum(
                body,
                "hl-signer-desktop-v0.1.0-signer-aarch64-apple-darwin.tar.gz"
            ),
            Some("deadbeefcafe".to_string())
        );
    }

    #[test]
    fn sha256_known_vector() {
        // `printf '' | sha256sum` → e3b0c4429...
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
