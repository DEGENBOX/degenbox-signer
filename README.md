# DegenBox HL Signer

Open-source, audit-grade Rust daemon that holds your **Hyperliquid API
agent key** locally and signs `/exchange` orders on your machine — so
the DegenBox web app can place perp trades for you without ever seeing
your secret.

> Web app: <https://staging.degenbox.app> · Install:
> <https://staging.degenbox.app/install>

The web app lives in a private monorepo. **This repo is everything that
touches your HL key** — so anyone running the binary can verify what it
does before pasting an API agent secret.

## What's in here

| Path | What it does |
| ---- | ------------ |
| `trading/hl-signer-desktop/` | The Rust daemon. Holds an Argon2id+AES-256-GCM-encrypted HL API agent secret, polls the DegenBox gateway for queued orders, signs EIP-712 locally, POSTs to Hyperliquid `/exchange`. Ships an interactive TUI (`hl-signer-desktop`) + headless mode (`hl-signer-desktop daemon`). |
| `vendor/platform-hl-exchange/` | Vendored `/exchange` client + EIP-712 signer. Same source as the private monorepo's `crates/platform/hl-exchange/`. |
| `.github/workflows/release.yml` | The same release matrix CI the private repo uses. Lets you reproduce the published binaries from source. |

### What is *not* in this repo (and why)

- **Solana signer / extension / Tauri GUI.** Those are still in
  development behind the `/install` page's "Beta soon" tiles. Once they
  ship publicly we will extend this repo to cover them too. Until then
  keeping the public attack surface to one binary keeps the audit story
  short.
- **The web app, gateway, scanner, admin panel.** Not key-material code
  — closed-source.

## Install

```sh
curl -fsSL https://degenbox.app/install-signer.sh | sh
```

Or download the archive for your platform from the latest
[GitHub Release](https://github.com/DEGENBOX/degenbox-signer/releases/latest),
extract, and add `hl-signer-desktop` to your `PATH`.

Then run:

```sh
hl-signer-desktop
```

The TUI walks you through encrypting your API agent secret, registering
the daemon with the DegenBox gateway, and starting the order loop.

## Verifying a release binary

Each tagged release ships:

- `hl-signer-desktop-<tag>-<target>.tar.gz` (per platform)
- `SHASUMS256.txt` listing the sha256 of every archive

To confirm the binary matches a tag in this repo:

```sh
# 1. Download the release archive + SHASUMS256.txt
shasum -a 256 -c SHASUMS256.txt --ignore-missing

# 2. Optionally, reproduce the binary from source at the same tag:
git clone https://github.com/DEGENBOX/degenbox-signer.git
cd degenbox-signer
git checkout v0.X.Y-signer
cargo build --release --locked -p hl-signer-desktop
shasum -a 256 target/release/hl-signer-desktop
```

Reproducible builds are best-effort — they require the same Rust
toolchain pinned in `rust-toolchain.toml` and the same target triple.
Differences in linker version, `SOURCE_DATE_EPOCH`, or `-C
codegen-units` can produce byte-different artifacts. The shipped CI
uses `dtolnay/rust-toolchain@stable` on `ubuntu-22.04` / `macos-14` /
`macos-15-intel` runners.

## Audit checklist — what to look at

The load-bearing files for a security review:

| Concern | File |
| ------- | ---- |
| Keystore format + Argon2id parameters | `trading/hl-signer-desktop/src/keystore.rs` |
| HL EIP-712 signing | `vendor/platform-hl-exchange/src/signer.rs` |
| HL `/exchange` POST transport | `vendor/platform-hl-exchange/src/client.rs` |
| Action types (order / cancel / leverage / approveAgent) | `vendor/platform-hl-exchange/src/actions.rs` |
| Signing pipeline (server-side msg → typed-data hash → sig) | `trading/hl-signer-desktop/src/signing.rs` |
| Polling / queue consumer | `trading/hl-signer-desktop/src/server.rs`, `daemon.rs` |
| Self-update verifier (sha256 against published manifest) | `trading/hl-signer-desktop/src/self_update.rs` |
| Address derivation from the secp256k1 secret | `trading/hl-signer-desktop/src/keystore.rs` + `signing.rs` |

### Things you should expect to find

- **No telemetry, no analytics.** The only HTTPS clients in the binary
  hit (a) the DegenBox gateway (poll for queued orders, post result),
  (b) Hyperliquid `/exchange` (submit the signed order), (c) Hyperliquid
  `/info` (account state lookup), (d) GitHub Releases (self-update).
- **No remote code execution.** Self-update verifies a sha256 against
  the release's `SHASUMS256.txt` before swapping the binary — no second
  loader, no eval, no `dlopen`.
- **Keystore lives only on disk.** Default path:
  `~/.config/degenbox/hl-keystore.json` (Linux),
  `~/Library/Application Support/degenbox/hl-keystore.json` (macOS),
  `%APPDATA%\degenbox\hl-keystore.json` (Windows).
- **NATS subscriber is optional.** The polling loop is always the
  source of truth; NATS is just a low-latency nudge that says "go poll
  now". If NATS is unreachable the loop still works, just slower.

If you find anything that contradicts the above, please report it (see
below).

## Known limitations

- **macOS binaries are currently unsigned.** Until we have an Apple
  Developer ID, the first launch shows a Gatekeeper warning; users
  either run `xattr -d com.apple.quarantine` or click "Open Anyway" in
  System Settings → Privacy & Security. Code-signing + notarization is
  on the roadmap.
- **Windows binaries are not yet built.** The CI matrix builds macOS
  arm64 + Intel + Linux x86_64 today; Windows users build from source
  with `cargo install --path trading/hl-signer-desktop` until the
  Windows runner job lands.
- **Reproducible builds are not byte-deterministic yet.** They are
  bit-stable across same-host rebuilds, but cross-host they may drift
  by a few bytes due to absolute-path strings baked into debug info.
- **This repo is push-only.** Pull requests opened here are reviewed
  but not merged directly — the canonical source is the private
  monorepo and changes flow private → public via the developer-side
  `sync-signer-public.sh` script.

## License

MIT. See `LICENSE`.

## Reporting security issues

Please **do not** open a public issue for a security vulnerability.
Email <security@degenbox.app> with:

- A description of the issue + reproduction steps
- The affected file path(s) in this repo
- Whether the issue is exploitable remotely or only with local access

We aim to acknowledge within 48 hours and ship a fix within 7 days for
critical issues. Coordinated disclosure preferred.
