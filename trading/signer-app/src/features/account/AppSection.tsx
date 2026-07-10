// 03 / APP — version + manual updater and the open-log-file action.
// Small and boring by design (iteration 3): execution params, copy
// budget, the paper-mode toggle and the "Source" link all left the
// Account surface — execution/budget moved into the Sol BOTS tab
// (features/bots/SolExecutionSettings), paper/live lives on the module
// Live status line, and an end-user app carries no code-repo link.

import { useEffect, useState } from "react";
import { FileText, RefreshCw } from "lucide-react";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { ipc } from "../../ipc";
import { Switch } from "../../components/ui";
import { getSkipCloseConfirm, setSkipCloseConfirm } from "../../lib/prefs";

export function AppSection() {
  const [version, setVersion] = useState("");
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  // "Confirm before closing" — inverse of the dbx.skipCloseConfirm pref.
  // This is the re-enable path once the operator has ticked "Don't ask
  // again" in a close dialog (which then hides the dialog entirely).
  const [confirmClose, setConfirmClose] = useState(() => !getSkipCloseConfirm());

  useEffect(() => {
    ipc.appVersion().then(setVersion).catch(() => {});
  }, []);

  const checkUpdate = async () => {
    setBusy(true);
    setMsg("Checking for updates…");
    try {
      const up = await check();
      if (up) {
        setMsg(`Update ${up.version} available. Downloading…`);
        await up.downloadAndInstall();
        await relaunch();
      } else {
        setMsg("You're on the latest version.");
      }
    } catch (e) {
      setMsg(`Update check failed: ${e}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="card">
      <div className="card-title">
        Application
        <span className="right hud-label">{version ? `v${version}` : ""}</span>
      </div>
      <div className="row">
        <span className="label">Updates</span>
        <span className="value">Manual in this build. Use the button below to check.</span>
      </div>
      <div className="row">
        <span className="label">Confirm before closing</span>
        <span className="value" style={{ display: "flex", alignItems: "center", gap: 8 }}>
          <Switch
            on={confirmClose}
            title={
              confirmClose
                ? "Ask for confirmation before closing/selling a position"
                : "Positions close directly with no confirmation (Perps + Solana)"
            }
            onToggle={(next) => {
              setConfirmClose(next);
              setSkipCloseConfirm(!next);
            }}
          />
          <span style={{ color: "var(--fg-faint)", fontSize: 12 }}>
            {confirmClose ? "on" : "off — one-click closes"}
          </span>
        </span>
      </div>
      {msg && <p style={{ marginTop: 10, marginBottom: 0 }}>{msg}</p>}
      <div className="btn-row">
        <button className="btn" disabled={busy} onClick={checkUpdate}>
          <RefreshCw size={14} /> Check for updates
        </button>
        <button className="btn" onClick={() => ipc.openLogs()}>
          <FileText size={14} /> Open log file
        </button>
      </div>
    </div>
  );
}
