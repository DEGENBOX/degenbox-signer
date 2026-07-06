// Full-screen unlock gate (W1 unlock-UX). Shown by App.tsx whenever a
// vault exists but is locked — the app never buries unlock inside
// Settings anymore. Locked decision: unlock ONCE per start, holds
// until app-close or access loss. There is no idle/timeout re-lock
// anywhere (audited W1) — lock happens only via the explicit button
// (Account tab), process exit, or the access-loss watcher in App.tsx.
//
// When the OS-keychain backend holds a cached passphrase, the Rust
// boot auto-unlock (commands::try_keychain_auto_unlock) usually wins
// before the first status poll — then this screen never appears. If
// the KDF is still running, the screen shows briefly and dismisses
// itself when the 2 s status poll sees the unlock land.
//
// `reason` is set on access-loss locks ("ACCESS REVOKED / SUB
// EXPIRED") and offers the Discord re-link path — that flow works
// pre-unlock (browser hand-off, no vault needed).

import { useEffect, useRef, useState } from "react";
import { ExternalLink, ShieldAlert, Unlock as UnlockIcon } from "lucide-react";
import { ipc } from "../ipc";

interface Props {
  /** HUD reason line for access-loss locks; null = normal start. */
  reason: string | null;
  /** Raw gateway detail under the reason line (optional). */
  reasonDetail?: string | null;
  onUnlocked: () => void;
}

export function Unlock({ reason, reasonDetail, onUnlocked }: Props) {
  const [password, setPassword] = useState("");
  const [remember, setRemember] = useState(true);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [relinkBusy, setRelinkBusy] = useState(false);
  const [resetBusy, setResetBusy] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  const submit = async () => {
    if (!password || busy) return;
    setBusy(true);
    setErr(null);
    try {
      await ipc.unlock(password, remember ? "keychain" : "file");
      setPassword("");
      onUnlocked();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const relink = async () => {
    setRelinkBusy(true);
    try {
      await ipc.discordLoginStart();
    } catch (e) {
      setErr(String(e));
    } finally {
      setRelinkBusy(false);
    }
  };

  // Forgot-passphrase / leftover-vault escape hatch: move the keystore aside
  // (backed up, not deleted) and reload so the setup wizard runs again.
  const resetVault = async () => {
    const ok = window.confirm(
      "Reset the vault and start fresh?\n\n" +
        "This moves your current keystore aside as a BACKUP (it is not deleted) " +
        "so the setup wizard runs again. Use this only if you've lost the " +
        "passphrase or this is a leftover install from a previous version.\n\n" +
        "The app will reload; if the setup screen doesn't appear, fully quit " +
        "and reopen it.",
    );
    if (!ok) return;
    setResetBusy(true);
    setErr(null);
    try {
      await ipc.resetKeystore();
      window.location.reload();
    } catch (e) {
      setErr(String(e));
      setResetBusy(false);
    }
  };

  return (
    <div className="unlock-screen">
      <div className="unlock-card corners">
        <div className="unlock-brand">
          <img src="/degenbox-logo.png" alt="" aria-hidden />
        </div>
        <h1>DegenBox</h1>
        <div className="unlock-hud">
          <span className="hud-label brackets">Vault locked</span>
        </div>

        {reason && (
          <div className="unlock-reason" role="alert">
            <ShieldAlert size={14} style={{ flexShrink: 0 }} />
            <span style={{ flex: 1 }}>
              {reason}
              {reasonDetail && <span className="detail">{reasonDetail}</span>}
            </span>
          </div>
        )}

        <div className="field-group">
          <label className="field" htmlFor="unlock-pass">
            Passphrase
          </label>
          <input
            id="unlock-pass"
            ref={inputRef}
            type="password"
            className="input"
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submit();
            }}
          />
        </div>

        <label className="check-row" style={{ marginBottom: 14 }}>
          <input
            type="checkbox"
            checked={remember}
            onChange={(e) => setRemember(e.target.checked)}
          />
          Cache passphrase in the OS keychain (auto-unlock next start)
        </label>

        {err && <div className="error-box">{err}</div>}

        {/* one glow focus per screen: .btn.primary already carries the
            restrained accent glow — no extra .glow-accent on top. */}
        <button
          className="btn primary lg"
          style={{ width: "100%", justifyContent: "center" }}
          disabled={busy || !password}
          onClick={submit}
        >
          <UnlockIcon size={15} /> {busy ? "Unlocking…" : "Unlock"}
        </button>

        {reason && (
          <div className="btn-row" style={{ marginTop: 10, justifyContent: "center" }}>
            <button className="btn discord-btn" disabled={relinkBusy} onClick={relink}>
              Re-link Discord <ExternalLink size={12} />
            </button>
          </div>
        )}

        <button
          type="button"
          disabled={resetBusy}
          onClick={resetVault}
          style={{
            marginTop: 14,
            background: "none",
            border: "none",
            color: "var(--ink-4, #6b7280)",
            fontSize: 11,
            cursor: "pointer",
            textDecoration: "underline",
            opacity: 0.7,
          }}
        >
          {resetBusy ? "Resetting…" : "Forgot passphrase? Reset & start fresh"}
        </button>

        <div className="unlock-foot">
          <span className="hud-label">AES-256-GCM · Argon2id · keys stay local</span>
        </div>
      </div>
    </div>
  );
}
