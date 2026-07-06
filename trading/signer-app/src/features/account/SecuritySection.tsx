// 02 / SECURITY — vault lock state + passphrase cache, and the
// multi-wallet key material (iteration 3). Signer pairing + 2FA LEFT
// this surface: pairing is Perpetuals-module content and now lives only
// on the Perpetuals · Bots executor card; a pending 2FA challenge is
// answered via the global banner.
//
// One passphrase guards every wallet; unlock is once per start (W1
// locked decision), so the only in-shell vault control is LOCK.
// `pick_backend` is a persistence stub (commands.rs) — the picker
// records the preference for the next unlock and says so.

import { useState } from "react";
import { Lock, ShieldCheck } from "lucide-react";
import { StatusPill } from "@degenbox/ui";
import { ipc, type StatusReport } from "../../ipc";
import { WalletsCard } from "./WalletsCard";

interface Props {
  status: StatusReport | null;
  onReload: () => void;
}

export function SecuritySection({ status, onReload }: Props) {
  return (
    <>
      <VaultCard status={status} onReload={onReload} />
      <WalletsCard status={status} onReload={onReload} />
    </>
  );
}

function VaultCard({ status, onReload }: Props) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  // Preference for the NEXT unlock — pick_backend doesn't persist yet,
  // so this is session-local; the unlock screen re-asks.
  const [backend, setBackend] = useState<"keychain" | "file">("keychain");

  const anyUnlocked = !!(status?.hl_unlocked || status?.sol_unlocked);
  const anyKeystore = !!(status?.hl_address || status?.sol_pubkey);

  const lock = async () => {
    setBusy(true);
    setErr(null);
    try {
      await ipc.lock();
      onReload(); // the App gate flips to the full-screen Unlock view
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const pickBackend = async (b: "keychain" | "file") => {
    setBackend(b);
    try {
      await ipc.pickBackend(b);
    } catch {
      // stub command — preference is applied at the next unlock anyway
    }
  };

  return (
    <div className="card">
      <div className="card-title">
        Vault
        <span className="right">
          <StatusPill tone={anyUnlocked ? "ok" : "muted"} icon={anyUnlocked ? ShieldCheck : Lock}>
            {anyUnlocked ? "unlocked" : "locked"}
          </StatusPill>
        </span>
      </div>

      <div className="row">
        <span className="label">Passphrase cache</span>
        <span className="value" style={{ display: "inline-flex", alignItems: "center", gap: 10 }}>
          <span className="acct-backend" role="radiogroup" aria-label="Passphrase cache backend">
            <button
              type="button"
              role="radio"
              aria-checked={backend === "keychain"}
              className={backend === "keychain" ? "active" : ""}
              onClick={() => pickBackend("keychain")}
            >
              OS keychain
            </button>
            <button
              type="button"
              role="radio"
              aria-checked={backend === "file"}
              className={backend === "file" ? "active" : ""}
              onClick={() => pickBackend("file")}
            >
              None
            </button>
          </span>
          <span className="hud-label">next unlock</span>
        </span>
      </div>

      <p style={{ marginTop: 10 }}>
        The vault unlocks once per start and stays open until app close or access loss
        (no idle re-lock). Locking wipes the decrypted secrets from this process and drops
        the cached passphrase; every runtime pauses until you unlock again.
      </p>
      {err && <div className="error-box">{err}</div>}
      <div className="btn-row" style={{ marginTop: 4 }}>
        <button
          className="btn danger"
          disabled={busy || !anyKeystore || !anyUnlocked}
          title={
            !anyKeystore
              ? "No keystore on this device yet"
              : !anyUnlocked
                ? "Already locked"
                : "Lock every keystore now"
          }
          onClick={lock}
        >
          <Lock size={14} /> Lock vault
        </button>
      </div>
    </div>
  );
}
