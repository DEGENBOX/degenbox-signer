// Solana wallet setup wizard — modal, focused, re-runnable.
//
// Paths (private-key paste is the PRIMARY one — it's what most users
// arrive with from Phantom / Solflare):
//   PASTE   raw base58/hex secret → passphrase → import (auto-unlock)
//   CREATE  passphrase → generate (auto-unlock) → forced backup
//           acknowledgement (export encrypted keystore / optional
//           secret reveal) → done
//   OTHER   detected signer-cli keystore | keystore file | extension
//           JSON → unlock → done
//
// The pasted secret is masked, never logged, and cleared from state the
// moment the import succeeds. The raw secret of a CREATED wallet never
// renders unless the user explicitly clicks "Reveal", and the backup
// step cannot be completed without either an exported keystore file or
// an acknowledged reveal.

import { useEffect, useState } from "react";
import {
  AlertTriangle,
  Check,
  Download,
  Eye,
  EyeOff,
  FileKey,
  KeyRound,
  Plus,
  Wallet,
} from "lucide-react";
import { open as openFileDialog, save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { ipc, type CliKeystoreInfo } from "../ipc";
import { CopyButton, Modal } from "./ui";

type Step = "mode" | "create" | "backup" | "import" | "secret" | "extension" | "unlock" | "done";

interface Props {
  open: boolean;
  onClose: () => void;
  /** Called after the wizard finished (wallet exists, ideally unlocked). */
  onDone: () => void;
  /** An HL keystore already exists → the passphrase must match it. */
  hasHlKeystore: boolean;
  /** A Solana wallet already exists → create/import would be refused by
   * the backend, so the wizard disables those paths with a reason. */
  hasSolWallet?: boolean;
}

export function SolanaWizard({ open, onClose, onDone, hasHlKeystore, hasSolWallet }: Props) {
  const [step, setStep] = useState<Step>("mode");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  // create path
  const [password, setPassword] = useState("");
  const [password2, setPassword2] = useState("");
  const [backend, setBackend] = useState<"file" | "keychain">("keychain");
  const [pubkey, setPubkey] = useState<string | null>(null);
  const [exported, setExported] = useState(false);
  const [secret, setSecret] = useState<string | null>(null);
  const [ack, setAck] = useState(false);

  // import paths
  const [cli, setCli] = useState<CliKeystoreInfo | null>(null);
  const [rawSecret, setRawSecret] = useState("");
  const [extJson, setExtJson] = useState("");
  const [unlockPw, setUnlockPw] = useState("");
  const [unlockSkippable, setUnlockSkippable] = useState(false);

  useEffect(() => {
    if (!open) return;
    setStep("mode");
    setBusy(false);
    setErr(null);
    setPassword("");
    setPassword2("");
    setPubkey(null);
    setExported(false);
    setSecret(null);
    setAck(false);
    setRawSecret("");
    setExtJson("");
    setUnlockPw("");
    ipc.detectCliKeystore().then(setCli).catch(() => setCli(null));
  }, [open]);

  const run = async (fn: () => Promise<void>) => {
    setBusy(true);
    setErr(null);
    try {
      await fn();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const passwordOk = password.length >= 8 && password === password2;

  const createWallet = () =>
    run(async () => {
      const res = await ipc.generateSolanaWallet(password);
      setPubkey(res.pubkey);
      // Unlock immediately so the runtimes arm without a second prompt.
      await ipc.unlock(password, backend);
      setStep("backup");
    });

  const exportKeystore = () =>
    run(async () => {
      const dest = await saveFileDialog({
        title: "Save encrypted keystore backup",
        defaultPath: "degenbox-sol-keystore.json",
        filters: [{ name: "Keystore", extensions: ["json"] }],
      });
      if (typeof dest === "string" && dest) {
        // Scope to the wallet THIS run created — with multiple vault
        // wallets the default would be the primary, not the new one.
        await ipc.exportSolKeystore(dest, pubkey ?? undefined);
        setExported(true);
      }
    });

  const revealSecret = () =>
    run(async () => {
      setSecret(await ipc.revealSolSecret(password, pubkey ?? undefined));
    });

  const importFile = (path: string) =>
    run(async () => {
      const res = await ipc.importSolKeystoreFile(path);
      setPubkey(res.pubkey);
      setUnlockSkippable(true);
      setStep("unlock");
    });

  const pickFile = async () => {
    const path = await openFileDialog({
      multiple: false,
      title: "Select a DegenBox signer keystore (keystore.json)",
      filters: [{ name: "Keystore", extensions: ["json"] }],
    });
    if (typeof path === "string" && path) await importFile(path);
  };

  const importExtension = () =>
    run(async () => {
      const res = await ipc.importExtensionKeystore(extJson, unlockPw);
      setPubkey(res.pubkey);
      // Same password — unlock straight away.
      await ipc.unlock(unlockPw, backend);
      setStep("done");
    });

  const importRawSecret = () =>
    run(async () => {
      const res = await ipc.importSolanaWallet(rawSecret.trim(), password);
      // The pasted secret has served its purpose — wipe it from React
      // state immediately so it can't linger in the field.
      setRawSecret("");
      setPubkey(res.pubkey);
      await ipc.unlock(password, backend);
      setStep("done");
    });

  const unlockNow = () =>
    run(async () => {
      await ipc.unlock(unlockPw, backend);
      setStep("done");
    });

  const finish = () => {
    onDone();
    onClose();
  };

  const stepIndex =
    step === "mode" ? 0 : step === "done" ? 2 : step === "backup" || step === "unlock" ? 2 : 1;

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={
        <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
          <span className="chain-dot sol" /> Solana wallet setup
        </span>
      }
      locked={busy || step === "backup"}
      width={520}
    >
      <StepStrip labels={["Wallet", "Details", "Finish"]} active={stepIndex} />

      {step === "mode" && (
        <>
          {hasSolWallet && (
            <div className="banner warn" role="status">
              <AlertTriangle size={15} style={{ flexShrink: 0 }} />
              <span>
                This device already has a Solana wallet. To switch to a different one,
                remove the current wallet first (Solana → Overview → Remove wallet…),
                then re-run this setup.
              </span>
            </div>
          )}
          <p>
            Add the hot wallet this device trades with. Bring your existing wallet, or
            create a fresh one. The key never leaves this machine.
          </p>
          <div className="choice-grid">
            <button
              className="choice featured"
              disabled={hasSolWallet}
              style={hasSolWallet ? { opacity: 0.55, cursor: "not-allowed" } : undefined}
              onClick={() => setStep("secret")}
            >
              <div className="title">
                <KeyRound size={16} /> Paste private key
                <span className="badge ok" style={{ marginLeft: "auto" }}>
                  recommended
                </span>
              </div>
              <p className="desc">
                Paste the secret key from Phantom / Solflare (base58 or hex). Encrypted
                under a passphrase on this machine. The fastest way in.
              </p>
            </button>
            <button
              className="choice"
              disabled={hasSolWallet}
              style={hasSolWallet ? { opacity: 0.55, cursor: "not-allowed" } : undefined}
              onClick={() => setStep("create")}
            >
              <div className="title">
                <Plus size={16} /> Create new wallet
              </div>
              <p className="desc">
                Generate a fresh keypair, encrypted under a passphrase you choose. Guided
                backup before you finish.
              </p>
            </button>
            <button
              className="choice"
              disabled={hasSolWallet}
              style={hasSolWallet ? { opacity: 0.55, cursor: "not-allowed" } : undefined}
              onClick={() => setStep("import")}
            >
              <div className="title">
                <Download size={16} /> More import options
              </div>
              <p className="desc">
                Keystore file, signer-cli wallet, or a browser-extension export.
              </p>
            </button>
          </div>
          {cli && !hasSolWallet && (
            <div className="banner info" role="status" style={{ marginTop: 14 }}>
              <FileKey size={15} style={{ flexShrink: 0 }} />
              <span style={{ flex: 1 }}>
                Found your signer-cli wallet{" "}
                <span className="mono">{short(cli.pubkey)}</span> on this machine.
              </span>
              <button
                className="btn primary"
                disabled={busy}
                onClick={() => importFile(cli.path)}
              >
                Import it
              </button>
            </div>
          )}
        </>
      )}

      {step === "create" && (
        <>
          <p>
            Pick the passphrase that encrypts your wallet on disk (AES-256-GCM, Argon2id).
            {hasHlKeystore && (
              <>
                {" "}
                <strong>
                  Use the same passphrase as your Hyperliquid key
                </strong>{" "}
                so the app unlocks both with one passphrase.
              </>
            )}
          </p>
          <div className="field-group">
            <label className="field">Passphrase (8+ characters)</label>
            <input
              type="password"
              className="input"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              autoFocus
            />
          </div>
          <div className="field-group">
            <label className="field">Confirm passphrase</label>
            <input
              type="password"
              className="input"
              value={password2}
              onChange={(e) => setPassword2(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && passwordOk && !busy) createWallet();
              }}
            />
          </div>
          <BackendPick backend={backend} onBackend={setBackend} />
          {err && <div className="error-box">{err}</div>}
          <div className="modal-foot">
            <button
              className="btn"
              style={{ marginRight: "auto" }}
              disabled={busy}
              onClick={() => setStep("mode")}
            >
              Back
            </button>
            <button
              className="btn primary"
              disabled={!passwordOk || busy}
              onClick={createWallet}
            >
              {busy ? "Creating…" : "Create wallet"}
            </button>
          </div>
        </>
      )}

      {step === "backup" && pubkey && (
        <>
          <div className="banner warn" role="alert">
            <AlertTriangle size={15} style={{ flexShrink: 0 }} />
            <span>
              <strong>Back up this wallet now.</strong> DegenBox cannot recover it. If
              this machine dies without a backup, funds on it are gone.
            </span>
          </div>
          <label className="field">Your new wallet address</label>
          <div className="pubkey-box">
            <span className="addr">{pubkey}</span>
            <CopyButton text={pubkey} label="Copy address" />
          </div>
          <div className="btn-row" style={{ marginTop: 4 }}>
            <button className="btn primary" disabled={busy} onClick={exportKeystore}>
              <Download size={14} /> {exported ? "Export again…" : "Export encrypted keystore…"}
            </button>
            <button
              className="btn"
              disabled={busy}
              onClick={() => (secret ? setSecret(null) : revealSecret())}
            >
              {secret ? <EyeOff size={14} /> : <Eye size={14} />}{" "}
              {secret ? "Hide secret key" : "Reveal secret key"}
            </button>
          </div>
          {exported && (
            <p style={{ marginTop: 8, color: "var(--accent)", display: "flex", gap: 6 }}>
              <Check size={14} /> Keystore exported. Store it somewhere safe (it stays
              encrypted under your passphrase).
            </p>
          )}
          {secret && (
            <>
              <div className="secret-box">{secret}</div>
              <div className="btn-row" style={{ marginTop: 0 }}>
                <CopyButton text={secret} label="Copy secret key" />
                <span style={{ fontSize: 11.5, color: "var(--fg-faint)", alignSelf: "center" }}>
                  Base58, importable into Phantom / Solflare. Never share it.
                </span>
              </div>
            </>
          )}
          <label className="check-row">
            <input
              type="checkbox"
              checked={ack}
              disabled={!exported && !secret}
              onChange={(e) => setAck(e.target.checked)}
            />
            <span>
              I have securely backed up this wallet and understand that DegenBox cannot
              restore it for me.
              {!exported && !secret && (
                <span style={{ display: "block", color: "var(--fg-faint)", fontSize: 11.5 }}>
                  Export the keystore (or reveal + save the secret) first.
                </span>
              )}
            </span>
          </label>
          {err && <div className="error-box">{err}</div>}
          <div className="modal-foot">
            <button className="btn primary" disabled={!ack || busy} onClick={() => setStep("done")}>
              Continue
            </button>
          </div>
        </>
      )}

      {step === "import" && (
        <>
          <p>How do you want to bring your wallet in?</p>
          <div className="choice-grid">
            <button className="choice" onClick={pickFile}>
              <div className="title">
                <FileKey size={16} /> Keystore file
              </div>
              <p className="desc">
                A DegenBox <span className="mono">keystore.json</span> (signer-cli or an
                exported app backup). Passphrase stays the same.
              </p>
            </button>
            <button className="choice" onClick={() => setStep("extension")}>
              <div className="title">
                <Download size={16} /> Browser extension
              </div>
              <p className="desc">
                Paste the keystore JSON exported from the DegenBox Chrome extension.
              </p>
            </button>
            {cli && (
              <button className="choice" onClick={() => importFile(cli.path)}>
                <div className="title">
                  <Wallet size={16} /> signer-cli wallet
                </div>
                <p className="desc">
                  Detected at <span className="mono">{cli.path}</span>. One-click import.
                </p>
              </button>
            )}
          </div>
          {err && <div className="error-box">{err}</div>}
          <div className="modal-foot">
            <button
              className="btn"
              style={{ marginRight: "auto" }}
              disabled={busy}
              onClick={() => setStep("mode")}
            >
              Back
            </button>
          </div>
        </>
      )}

      {step === "secret" && (
        <>
          <p>
            Paste the raw secret key (base58 or hex, 32 or 64 bytes), then choose the
            passphrase that encrypts it on this machine.
            {hasHlKeystore && (
              <>
                {" "}
                <strong>Use the same passphrase as your Hyperliquid key.</strong>
              </>
            )}
          </p>
          <div className="field-group">
            <label className="field">Secret key</label>
            <input
              type="password"
              className="input mono"
              value={rawSecret}
              onChange={(e) => setRawSecret(e.target.value)}
              placeholder="base58 or hex"
              autoFocus
            />
          </div>
          <div className="field-group">
            <label className="field">Passphrase (8+ characters)</label>
            <input
              type="password"
              className="input"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
            />
          </div>
          <div className="field-group">
            <label className="field">Confirm passphrase</label>
            <input
              type="password"
              className="input"
              value={password2}
              onChange={(e) => setPassword2(e.target.value)}
            />
          </div>
          <BackendPick backend={backend} onBackend={setBackend} />
          {err && <div className="error-box">{err}</div>}
          <div className="modal-foot">
            <button
              className="btn"
              style={{ marginRight: "auto" }}
              disabled={busy}
              onClick={() => setStep("mode")}
            >
              Back
            </button>
            <button
              className="btn primary"
              disabled={!rawSecret.trim() || !passwordOk || busy}
              onClick={importRawSecret}
            >
              {busy ? "Importing…" : "Import wallet"}
            </button>
          </div>
        </>
      )}

      {step === "extension" && (
        <>
          <p>
            Paste the keystore JSON exported from the DegenBox extension and the password
            it was encrypted with. It is re-encrypted into the app&apos;s native format
            under the same password.
          </p>
          <div className="field-group">
            <label className="field">Extension keystore JSON</label>
            <textarea
              className="input mono"
              rows={4}
              value={extJson}
              onChange={(e) => setExtJson(e.target.value)}
              placeholder='{"v":1,"pubkey":"…","kdf":"argon2id",…}'
            />
          </div>
          <div className="field-group">
            <label className="field">Its password</label>
            <input
              type="password"
              className="input"
              value={unlockPw}
              onChange={(e) => setUnlockPw(e.target.value)}
            />
          </div>
          <BackendPick backend={backend} onBackend={setBackend} />
          {err && <div className="error-box">{err}</div>}
          <div className="modal-foot">
            <button
              className="btn"
              style={{ marginRight: "auto" }}
              disabled={busy}
              onClick={() => setStep("import")}
            >
              Back
            </button>
            <button
              className="btn primary"
              disabled={!extJson.trim() || !unlockPw || busy}
              onClick={importExtension}
            >
              {busy ? "Importing…" : "Decrypt & import"}
            </button>
          </div>
        </>
      )}

      {step === "unlock" && (
        <>
          <p>
            Wallet <span className="mono">{pubkey ? short(pubkey) : ""}</span> imported.
            Unlock it now so this device can start signing.
          </p>
          <div className="field-group">
            <label className="field">Passphrase</label>
            <input
              type="password"
              className="input"
              value={unlockPw}
              onChange={(e) => setUnlockPw(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && unlockPw && !busy) unlockNow();
              }}
              autoFocus
            />
          </div>
          <BackendPick backend={backend} onBackend={setBackend} />
          {err && <div className="error-box">{err}</div>}
          <div className="modal-foot">
            {unlockSkippable ? (
              <button
                className="btn"
                style={{ marginRight: "auto" }}
                disabled={busy}
                onClick={() => setStep("done")}
              >
                Unlock later
              </button>
            ) : (
              <span style={{ marginRight: "auto" }} />
            )}
            <button className="btn primary" disabled={!unlockPw || busy} onClick={unlockNow}>
              {busy ? "Unlocking…" : "Unlock"}
            </button>
          </div>
        </>
      )}

      {step === "done" && (
        <>
          <div className="banner info" role="status">
            <Check size={15} style={{ flexShrink: 0 }} />
            <span>
              <strong>Solana wallet ready.</strong>{" "}
              {pubkey ? (
                <span className="mono">{short(pubkey)}</span>
              ) : (
                "Wallet configured."
              )}{" "}
              TP/SL sells run automatically while unlocked; copy buys stay disarmed until
              you set a session budget on the Solana → Wallet page.
            </span>
          </div>
          <div className="modal-foot">
            <button className="btn primary" onClick={finish}>
              Done
            </button>
          </div>
        </>
      )}
    </Modal>
  );
}

export function BackendPick({
  backend,
  onBackend,
}: {
  backend: "file" | "keychain";
  onBackend: (b: "file" | "keychain") => void;
}) {
  return (
    <div className="field-group" style={{ marginBottom: 10 }}>
      <label className="field">Cache passphrase in the OS keychain?</label>
      <div style={{ display: "flex", gap: 8 }}>
        <button
          className={`btn ${backend === "keychain" ? "primary" : ""}`}
          onClick={() => onBackend("keychain")}
          type="button"
        >
          Yes (recommended)
        </button>
        <button
          className={`btn ${backend === "file" ? "primary" : ""}`}
          onClick={() => onBackend("file")}
          type="button"
        >
          No, ask every time
        </button>
      </div>
    </div>
  );
}

export function StepStrip({ labels, active }: { labels: string[]; active: number }) {
  return (
    <div className="wstep-strip">
      {labels.map((l, i) => (
        <span key={l} style={{ display: "contents" }}>
          {i > 0 && <span className="wstep-sep" />}
          <span className={`wstep ${i === active ? "active" : i < active ? "done" : ""}`}>
            <span className="pip">{i < active ? <Check size={11} /> : i + 1}</span>
            {l}
          </span>
        </span>
      ))}
    </div>
  );
}

function short(s: string) {
  return s.length > 16 ? `${s.slice(0, 6)}…${s.slice(-6)}` : s;
}
