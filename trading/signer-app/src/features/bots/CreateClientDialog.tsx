// Create-client wizard-lite: name → wallet source (generate / paste a
// private key / adopt an extension keystore) → master passphrase →
// done (+ backup nudge for generated keys). Logic ported from the old
// AddClientDialog, Sol-only, rebuilt in the blackalgo language; the
// attach matrix itself lives in features/bots/ipc.ts (attachClient).

import { useEffect, useState } from "react";
import { Check, Download, FileJson, KeyRound, Plus, Sparkles } from "lucide-react";
import { save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { shortAddr } from "@degenbox/ui";
import { Modal } from "../../components/ui";
import { attachClient, ipc, type AttachMethod } from "./ipc";

interface Props {
  open: boolean;
  onClose: () => void;
  /** Vault changed — owner re-polls the fleet. */
  onDone: () => void;
}

export function CreateClientDialog({ open, onClose, onDone }: Props) {
  const [method, setMethod] = useState<AttachMethod>("generate");
  const [label, setLabel] = useState("");
  const [secret, setSecret] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [warn, setWarn] = useState<string | null>(null);
  const [created, setCreated] = useState<{ id: string | null; address: string } | null>(
    null,
  );
  const [exported, setExported] = useState(false);

  useEffect(() => {
    if (!open) return;
    setMethod("generate");
    setLabel("");
    setSecret("");
    setPassword("");
    setBusy(false);
    setErr(null);
    setWarn(null);
    setCreated(null);
    setExported(false);
  }, [open]);

  const submit = async () => {
    setBusy(true);
    setErr(null);
    setWarn(null);
    try {
      const r = await attachClient(method, {
        secret,
        label: label.trim(),
        password,
      });
      setSecret(""); // wipe the pasted key/blob immediately
      setPassword(""); // wipe the passphrase once activation ran
      if (r.activationError) {
        setWarn(
          `Client added, but its runtime could not start: ${r.activationError}. ` +
            "It will come online on the next unlock.",
        );
      }
      setCreated({ id: r.id, address: r.address });
      onDone();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const exportBackup = async () => {
    if (!created?.id) return;
    setBusy(true);
    setErr(null);
    try {
      const dest = await saveFileDialog({
        title: "Save encrypted keystore backup",
        defaultPath: `degenbox-sol-${created.address.slice(0, 6)}.json`,
        filters: [{ name: "Keystore", extensions: ["json"] }],
      });
      if (typeof dest === "string" && dest) {
        await ipc.clientExportKeystore(created.id, dest);
        setExported(true);
      }
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const canSubmit =
    password.length > 0 && (method === "generate" || secret.trim().length > 0) && !busy;

  const METHODS: { id: AttachMethod; icon: typeof KeyRound; title: string; desc: string }[] = [
    {
      id: "generate",
      icon: Sparkles,
      title: "Generate",
      desc: "Fresh wallet, created inside the vault.",
    },
    {
      id: "paste",
      icon: KeyRound,
      title: "Import key",
      desc: "Base58 or hex private key (32/64 bytes): Phantom, Solflare, CLI.",
    },
    {
      id: "extension",
      icon: FileJson,
      title: "Signer wallet",
      desc: "Re-attach a DegenBox extension keystore (exported JSON).",
    },
  ];

  return (
    <Modal
      open={open}
      onClose={() => (busy ? undefined : onClose())}
      title={
        <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
          <Plus size={14} /> New client
        </span>
      }
      width={520}
      locked={busy}
    >
      {created ? (
        <>
          <div className="done-pop">
            <Check size={20} />
          </div>
          <p style={{ textAlign: "center", marginBottom: 4 }}>
            <strong style={{ color: "var(--fg)" }}>{label.trim() || "Client"} added</strong>
          </p>
          <p className="mono" style={{ textAlign: "center" }}>
            {shortAddr(created.address, 8, 8)}
          </p>

          {method === "generate" && (
            <div className="banner warn" role="alert" style={{ marginTop: 10 }}>
              <span>
                This key exists <strong>only on this machine</strong>. Export the
                encrypted keystore now. Without a backup, losing this device loses the
                wallet.
              </span>
            </div>
          )}
          {warn && <div className="banner warn">{warn}</div>}
          {err && <div className="error-box">{err}</div>}

          <div className="btn-row" style={{ justifyContent: "center" }}>
            {method === "generate" && created.id && (
              <button className="btn" disabled={busy} onClick={exportBackup}>
                <Download size={13} /> {exported ? "Exported ✓" : "Export backup…"}
              </button>
            )}
            <button className="btn primary" disabled={busy} onClick={onClose}>
              Done
            </button>
          </div>
        </>
      ) : (
        <>
          <div className="hud-label" style={{ marginBottom: 8 }}>
            Wallet source
          </div>
          <div className="choice-grid" style={{ marginTop: 0, marginBottom: 14 }}>
            {METHODS.map(({ id, icon: Icon, title, desc }) => (
              <button
                key={id}
                type="button"
                className={`choice ${method === id ? "selected" : ""}`}
                onClick={() => setMethod(id)}
              >
                <div className="title">
                  <Icon size={13} /> {title}
                </div>
                <p className="desc">{desc}</p>
              </button>
            ))}
          </div>

          {method !== "generate" && (
            <div className="field-group">
              <label className="field">
                {method === "paste"
                  ? "Private key (base58 / hex)"
                  : "Extension keystore JSON"}
              </label>
              {method === "paste" ? (
                <input
                  type="password"
                  className="input mono"
                  value={secret}
                  onChange={(e) => setSecret(e.target.value)}
                  placeholder="paste here (never leaves this machine)"
                  autoFocus
                />
              ) : (
                <textarea
                  className="input mono"
                  style={{ minHeight: 72, resize: "vertical" }}
                  value={secret}
                  onChange={(e) => setSecret(e.target.value)}
                  placeholder='{"version":…} (exported from the DegenBox extension)'
                  autoFocus
                />
              )}
              {method === "extension" && (
                <div style={{ fontSize: 11, color: "var(--fg-faint)", marginTop: 4 }}>
                  the blob must be encrypted under the same master passphrase as this
                  vault
                </div>
              )}
            </div>
          )}

          <div className="field-group">
            <label className="field">Client name (optional)</label>
            <input
              className="input"
              value={label}
              onChange={(e) => setLabel(e.target.value)}
              placeholder="e.g. Sniper 2"
              autoFocus={method === "generate"}
              onKeyDown={(e) => {
                if (e.key === "Enter" && canSubmit) submit();
              }}
            />
          </div>

          <div className="field-group">
            <label className="field">Master passphrase</label>
            <input
              type="password"
              className="input"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="the one vault passphrase"
              onKeyDown={(e) => {
                if (e.key === "Enter" && canSubmit) submit();
              }}
            />
          </div>

          {err && <div className="error-box">{err}</div>}
          <div className="modal-foot">
            <button className="btn" disabled={busy} onClick={onClose}>
              Cancel
            </button>
            <button className="btn primary" disabled={!canSubmit} onClick={submit}>
              {busy ? "Adding…" : method === "generate" ? "Generate client" : "Add client"}
            </button>
          </div>
        </>
      )}
    </Modal>
  );
}
