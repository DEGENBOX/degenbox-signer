// Activate-runtime prompt — the "re-attach" that actually exists:
// client_activate(id, password) decrypts EXACTLY this vault wallet and
// starts its runtime while the app stays unlocked (idempotent for
// already-live ids; a locked app routes through the full unlock).

import { useEffect, useState } from "react";
import { KeyRound } from "lucide-react";
import { shortAddr } from "@degenbox/ui";
import { Modal } from "../../components/ui";
import { ipc, type ClientInfo } from "./ipc";

interface Props {
  client: ClientInfo | null;
  onClose: () => void;
  onDone: () => void;
}

export function ActivateDialog({ client, onClose, onDone }: Props) {
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    setPassword("");
    setErr(null);
    setBusy(false);
  }, [client]);

  if (!client) return null;

  const submit = async () => {
    setBusy(true);
    setErr(null);
    try {
      await ipc.clientActivate(client.id, password);
      setPassword("");
      onDone();
      onClose();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open
      onClose={() => (busy ? undefined : onClose())}
      title={
        <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
          <KeyRound size={14} /> Activate runtime
        </span>
      }
      width={420}
      locked={busy}
    >
      <p style={{ marginTop: 0 }}>
        Bring <strong>{client.label ?? shortAddr(client.address, 6, 6)}</strong>{" "}
        <span className="mono">({shortAddr(client.address, 5, 5)})</span> online now.
        Without this it idles until the next lock/unlock cycle.
      </p>
      <div className="field-group">
        <label className="field">Master passphrase</label>
        <input
          type="password"
          className="input"
          value={password}
          autoFocus
          onChange={(e) => setPassword(e.target.value)}
          placeholder="the one vault passphrase"
          onKeyDown={(e) => {
            if (e.key === "Enter" && password.length > 0 && !busy) submit();
          }}
        />
      </div>
      {err && <div className="error-box">{err}</div>}
      <div className="modal-foot">
        <button className="btn" disabled={busy} onClick={onClose}>
          Cancel
        </button>
        <button
          className="btn primary"
          disabled={busy || password.length === 0}
          onClick={submit}
        >
          {busy ? "Activating…" : "Activate"}
        </button>
      </div>
    </Modal>
  );
}
