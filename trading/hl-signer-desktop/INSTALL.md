# Install `hl-signer-desktop`

The DegenBox unified local desktop signer:

- **Hyperliquid** — holds your encrypted HL API agent key locally and
  signs queued instructions from the DegenBox server (`setup` /
  `register` / `daemon` / the interactive TUI).
- **Solana** — holds your encrypted Solana hot wallet, executes TP/SL
  sell triggers + copy-trade commands, and serves the local
  `127.0.0.1:5829` bridge so the DegenBox web app can use this signer
  (`sol init` / `sol import` / `sol daemon` / the TUI's Solana tab).

Both keystores live under `~/.config/degenbox/` and are shared with
the DegenBox Signer desktop app — you can flip between the CLI and the
app without re-importing. The standalone Solana `signer-cli` is
deprecated in favour of this binary.

## Multi-wallet vault (shared with the desktop app)

N Solana + N Hyperliquid wallets under ONE master password, stored in
`~/.config/degenbox/vault/` — the exact directory and format the
desktop app uses, so wallets added in either place appear in both.
Legacy single keystores are adopted into the vault automatically on
first use (originals kept as `.bak`).

```sh
hl-signer-desktop clients add  --chain sol --label "main"   # generate (creates the vault)
hl-signer-desktop clients import --chain hl --label "agent" # paste an HL API agent key
hl-signer-desktop clients list [--json]                     # vault + server registry merged
hl-signer-desktop clients pause <id|label|address>          # per-client kill-switch
hl-signer-desktop clients resume <…>
hl-signer-desktop clients set-primary <…>                   # which wallet executes
hl-signer-desktop clients label <…> "new name"
hl-signer-desktop clients remove <…>                        # keystore kept as .removed.bak
```

Run the whole fleet headless (the equivalent of the unlocked desktop
app — HL primary executes, other HL wallets keep a standby heartbeat,
the Solana primary runs TP/SL + copy streams and the `:5829` bridge):

```sh
hl-signer-desktop run [--password-stdin]   # or $DEGENBOX_MASTER_PASSWORD
```

The interactive TUI gets the same fleet on its **Clients** tab (tab 4):
add / import / pause / set-primary / label / remove.

## Discord login (headless)

```sh
hl-signer-desktop login     # prints a URL — open it in ANY browser (your laptop is fine)
```

After authorizing, the browser is redirected to a
`degenbox://auth/callback?code=…` link. Without the desktop app
installed that link opens nothing — copy the URL (or just the `code=`
value) from the address bar and paste it back into the terminal.
Firefox shows the `degenbox://` URL in the address bar; Chrome may only
show an "open app?" dialog, so use Firefox if you can't see the code.
The minted token is stored in `~/.config/degenbox/desktop-auth.json`
(shared with the desktop app) and feeds the Solana runtime + `clients`
commands automatically. `hl-signer-desktop account` shows the link,
`logout` removes it.

## Verify the source

The signer source is mirrored in a public, audit-grade repo:

- <https://github.com/DEGENBOX/degenbox-signer>

Before trusting a release binary with your HL API agent key, you can:

1. **Verify the published archive's sha256** against the
   `SHASUMS256.txt` file shipped on the same
   [GitHub Release](https://github.com/DEGENBOX/degenbox-signer/releases):

   ```sh
   shasum -a 256 -c SHASUMS256.txt --ignore-missing
   ```

2. **Reproduce the binary from source** at the same tag and compare:

   ```sh
   git clone https://github.com/DEGENBOX/degenbox-signer.git
   cd degenbox-signer
   git checkout v0.X.Y-signer
   cargo build --release --locked -p hl-signer-desktop
   shasum -a 256 target/release/hl-signer-desktop
   ```

   Reproducible builds are best-effort and may diverge by a few bytes
   between hosts (linker / debug-info paths). The sha-comparison above
   is the canonical integrity check.

3. **Read the keystore code** at
   `trading/hl-signer-desktop/src/keystore.rs` and the EIP-712 signing
   path at `vendor/platform-hl-exchange/src/agent.rs` to confirm the
   crypto envelope matches what's documented.

## Quick install (macOS / Linux)

```sh
curl -fsSL https://degenbox.app/install-signer.sh | sh
```

This downloads the prebuilt binary for your platform from the latest
GitHub release, verifies its sha256 checksum, and drops it into
`~/.degenbox/bin/`. (The Solana `signer-cli` is in beta and not yet
shipped publicly — only `hl-signer-desktop` is installed today.)

Then:

```sh
hl-signer-desktop setup        # Hyperliquid agent key
hl-signer-desktop sol init     # Solana hot wallet (optional)
```

Solana quick reference:

```sh
hl-signer-desktop sol import --file ~/.degenbox/keystore.json   # adopt a signer-cli keystore
hl-signer-desktop sol import --extension-json export.json      # adopt a Chrome-extension export
hl-signer-desktop sol budget --session-sol 0.5                 # REQUIRED before copy buys fire
hl-signer-desktop sol daemon                                   # headless executor + :5829 bridge
```

## Manual install

If you'd rather not pipe a script to a shell, download the archive
for your platform from the [GitHub Releases page](https://github.com/DEGENBOX/degenbox-signer/releases):

| Platform        | Asset                                                          |
| --------------- | -------------------------------------------------------------- |
| macOS (Apple)   | `hl-signer-desktop-<version>-aarch64-apple-darwin.tar.gz`      |
| macOS (Intel)   | `hl-signer-desktop-<version>-x86_64-apple-darwin.tar.gz`       |
| Linux x86_64    | `hl-signer-desktop-<version>-x86_64-unknown-linux-gnu.tar.gz`  |
| Windows x86_64  | `hl-signer-desktop-<version>-x86_64-pc-windows-msvc.zip`       |

```sh
tar -xzf hl-signer-desktop-<version>-<target>.tar.gz
cd hl-signer-desktop-<version>-<target>
chmod +x hl-signer-desktop
sudo mv hl-signer-desktop /usr/local/bin/    # or add this dir to PATH
```

Each release also ships `SHASUMS256.txt`; verify before installing:

```sh
shasum -a 256 -c SHASUMS256.txt --ignore-missing
```

### Windows note

There is no MSI yet. Unzip the archive, then add the folder containing
`hl-signer-desktop.exe` to your PATH (System Properties → Environment
Variables → Path → Edit). A signed installer is planned once we have
Apple + Windows code-signing certificates.

### macOS unsigned-binary warning

Until we have an Apple Developer ID, macOS Gatekeeper refuses to launch
the binary on first run with:

> "hl-signer-desktop" konnte nicht geöffnet werden, da Apple es nicht
> auf schädliche Software überprüfen kann.

**Two ways to fix it:**

**A) One-line terminal command** (fastest):

```sh
xattr -d com.apple.quarantine ~/Downloads/hl-signer-desktop-*/hl-signer-desktop
```

Adjust the path to wherever you unzipped the archive. Then just run
the binary normally:

```sh
cd ~/Downloads/hl-signer-desktop-v0.1.0-signer-aarch64-apple-darwin
./hl-signer-desktop setup
```

**B) System Settings UI**:

1. Open **System Settings → Privacy & Security**
2. Scroll down — you'll see a yellow banner: "hl-signer-desktop wurde
   blockiert"
3. Click **"Trotzdem öffnen"** / **"Open Anyway"**
4. Re-run the binary; macOS will ask once more, click **"Öffnen"**

After either method the binary runs without further prompts.

Code-signing + notarization is on the post-launch backlog (BACKLOG.md).

## Build from source (developers only)

From the public signer repo (recommended — no monorepo deps needed):

```sh
git clone https://github.com/DEGENBOX/degenbox-signer.git
cd degenbox-signer
cargo install --path trading/hl-signer-desktop
```

Requires the Rust toolchain (`rustup default stable`). Build time is
about 5–10 minutes on a first run.
