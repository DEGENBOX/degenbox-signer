# Install DegenBox Signer

The native desktop app that holds your Hyperliquid + Solana trading keys
locally and signs orders queued by the DegenBox cloud.

## Download

Grab the latest installer for your platform from the matching `v*-app`
release on the [DegenBox release page](https://github.com/DEGENBOX/degenbox-v2/releases)
(the CLI signer binaries ship separately under `v*-signer` tags).

| Platform                | File                                                |
| ----------------------- | --------------------------------------------------- |
| macOS (Apple Silicon)   | `hl-trader-x.y.z-aarch64-apple-darwin.dmg`          |
| macOS (Intel)           | `hl-trader-x.y.z-x86_64-apple-darwin.dmg`           |
| Windows (MSI)           | `hl-trader-x.y.z-x86_64-pc-windows-msvc.msi`        |
| Windows (NSIS setup)    | `hl-trader-x.y.z-x86_64-pc-windows-msvc-setup.exe`  |
| Linux (AppImage)        | `hl-trader-x.y.z-x86_64-unknown-linux-gnu.AppImage` |
| Linux (Debian/Ubuntu)   | `hl-trader-x.y.z-x86_64-unknown-linux-gnu.deb`      |

(The release also carries the bundler's original `DegenBox Signer_…` files —
same bits, brand-named.)

## Install

### macOS

1. Open the `.dmg`.
2. Drag **DegenBox Signer** into your **Applications** folder.
3. First launch: right-click the app → **Open**, then click **Open** in the warning dialog.

> macOS warns about apps from "unidentified developers" because we are not yet
> notarized with Apple. Allow it once and macOS remembers your choice.

### Windows

1. Run the `.msi` installer (or the NSIS `-setup.exe`, same app).
2. If SmartScreen warns ("Windows protected your PC"), click **More info →
   Run anyway** — the installers are not Authenticode-signed yet.
3. Follow the prompts. The installer adds DegenBox Signer to your Start menu.

### Linux

**AppImage (any distro):**

```
chmod +x degenbox-signer_*.AppImage
./degenbox-signer_*.AppImage
```

**Debian / Ubuntu:**

```
sudo dpkg -i degenbox-signer_*.deb
```

## First run

1. Launch **DegenBox Signer**.
2. The onboarding wizard walks you through:
   - Picking a passphrase for your encrypted keystore.
   - Importing your Hyperliquid API agent key (mint one at
     [app.hyperliquid.xyz/API](https://app.hyperliquid.xyz/API)).
   - Generating a fresh Solana hot-wallet.
   - Choosing storage (OS keychain recommended, or encrypted file only).
   - Connecting to DegenBox with an API token from your dashboard.

3. After setup, the app sits in your menu bar / system tray and signs orders
   automatically. The colored dot is the daemon's health:

   - Green: signing
   - Amber: degraded (transient server/network issue, retrying)
   - Red: locked or no key — open the app to unlock

## Update

Auto-update is **disabled** in the current unsigned preview builds (it
returns with signed releases). To update, download the new installer from the
latest `v*-app` release and install it over the existing app — your keystores
and settings are kept (they live outside the app bundle, see below).

## Where your data lives

| What            | Path                                                |
| --------------- | --------------------------------------------------- |
| Encrypted keys  | `~/.config/degenbox/hl-keystore.json` + `sol-keystore.json` (macOS/Linux) |
|                 | `%APPDATA%\degenbox\` (Windows)                     |
| Logs            | `~/.config/degenbox/signer-app.log`                 |
| Passphrase cache | OS keychain (only if you opted in during setup)    |

Your encrypted keystore is a single JSON file. Back it up like you would a
hardware-wallet seed — losing it means losing access to that wallet.

## Uninstall

- macOS: drag **DegenBox Signer** from Applications to Trash.
- Windows: Settings → Apps → DegenBox Signer → Uninstall.
- Linux: `sudo dpkg -r degenbox-signer` (or delete the AppImage).

Your `~/.config/degenbox/` directory is left in place so a reinstall picks up
your existing keys. Delete it manually if you want a clean slate.
