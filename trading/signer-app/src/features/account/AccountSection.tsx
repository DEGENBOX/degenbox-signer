// 01 / ACCOUNT — Discord identity + gateway access (W3.4 rebuild of
// the old Account page). The Discord link is THE primary auth path:
// one browser hand-off mints a gateway token that feeds both runtimes.
//
// Access status surfaces what `access_check` actually returns
// (ok / no_auth / revoked / unreachable — src-tauri/src/auth.rs),
// including the passed-through `/auth/me` claims (roles, exp) when the
// probe succeeds. Session expiry comes from the stored desktop JWT
// (DiscordStatus.expires_at); the roles row is the gateway's live
// answer.

import { useCallback, useEffect, useState } from "react";
import { ExternalLink, RefreshCw, Unlink, User } from "lucide-react";
import { StatusPill, type StatusPillTone } from "@degenbox/ui";
import { discordAvatarUrl, ipc, type AccessCheck, type DiscordStatus } from "../../ipc";
import { DangerConfirm } from "../../components/ui";

const ACCESS_PILL: Record<string, { tone: StatusPillTone; label: string }> = {
  ok: { tone: "ok", label: "access ok" },
  no_auth: { tone: "muted", label: "no credentials" },
  revoked: { tone: "danger", label: "revoked" },
  unreachable: { tone: "warn", label: "unreachable" },
};

export function AccountSection() {
  const [discord, setDiscord] = useState<DiscordStatus | null>(null);
  const [loadErr, setLoadErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [unlinkOpen, setUnlinkOpen] = useState(false);

  // Polled at 2s so the deep-link callback's result appears live,
  // including when the app was cold-started by the link.
  useEffect(() => {
    let alive = true;
    const load = async () => {
      try {
        const d = await ipc.discordStatus();
        if (alive) {
          setDiscord(d);
          setLoadErr(null);
        }
      } catch (e) {
        if (alive) setLoadErr(String(e));
      }
    };
    load();
    const id = setInterval(load, 2000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  // Gateway access probe — a real network round-trip to /auth/me, so
  // it runs once on mount + on explicit re-check only (the App shell
  // already polls it every 5 min for the access-loss lock).
  const [access, setAccess] = useState<AccessCheck | null>(null);
  const [accessBusy, setAccessBusy] = useState(false);
  const probeAccess = useCallback(async () => {
    setAccessBusy(true);
    try {
      setAccess(await ipc.accessCheck());
    } catch (e) {
      setAccess({ state: "unreachable", detail: String(e) });
    } finally {
      setAccessBusy(false);
    }
  }, []);
  useEffect(() => {
    probeAccess();
  }, [probeAccess]);

  const startLogin = async () => {
    setBusy(true);
    setErr(null);
    try {
      await ipc.discordLoginStart();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const unlink = async () => {
    setBusy(true);
    setErr(null);
    try {
      await ipc.discordUnlink();
      setUnlinkOpen(false);
      probeAccess();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const linked = !!discord?.linked;
  const avatar = discord ? discordAvatarUrl(discord) : null;
  const accessPill = access ? ACCESS_PILL[access.state] : null;

  return (
    <>
      {loadErr && <div className="error-box">{loadErr}</div>}

      <div className="card">
        <div className="card-title">
          Identity
          <span className="right">
            {linked ? (
              discord!.expired ? (
                <span className="badge warn">expired</span>
              ) : (
                <span className="badge ok">linked</span>
              )
            ) : (
              <span className="badge">not linked</span>
            )}
          </span>
        </div>

        {linked ? (
          <>
            <div className="acct-id">
              {avatar ? (
                <img className="avatar" src={avatar} alt="" aria-hidden />
              ) : (
                <span className="avatar placeholder">
                  <User size={18} />
                </span>
              )}
              <div className="who">
                <div className="name">{discord!.username}</div>
                <div className="sub">{discord!.discord_id}</div>
              </div>
            </div>

            <div className="row">
              <span className="label">Session expires</span>
              <span className="value">
                {discord!.expires_at ? new Date(discord!.expires_at).toLocaleString() : "—"}
                {discord!.expired && (
                  <span className="badge warn" style={{ marginLeft: 8 }}>
                    re-link needed
                  </span>
                )}
              </span>
            </div>
            <div className="row">
              <span className="label">Connection</span>
              <span
                className="value"
                style={{ display: "inline-flex", alignItems: "center", gap: 8 }}
              >
                {accessPill ? (
                  <span title={access?.detail ?? undefined}>
                    <StatusPill tone={accessPill.tone}>{accessPill.label}</StatusPill>
                  </span>
                ) : (
                  <span className="hud-label">probing…</span>
                )}
                <button
                  className="btn icon"
                  disabled={accessBusy}
                  title="Re-check gateway access (GET /auth/me)"
                  aria-label="Re-check gateway access"
                  onClick={probeAccess}
                >
                  <RefreshCw size={12} />
                </button>
              </span>
            </div>
            {access?.state === "ok" && (access.me?.roles?.length ?? 0) > 0 && (
              <div className="row">
                <span className="label">Roles</span>
                <span className="value" style={{ display: "inline-flex", gap: 6, flexWrap: "wrap" }}>
                  {access.me!.roles!.map((r) => (
                    <span key={r} className="badge mono">
                      {r}
                    </span>
                  ))}
                </span>
              </div>
            )}
            {access?.detail && access.state !== "ok" && (
              <p style={{ fontSize: 11.5, color: "var(--fg-faint)", margin: "6px 0 0" }}>
                {access.detail}
              </p>
            )}

            {err && <div className="error-box">{err}</div>}
            <div className="btn-row">
              <button className="btn discord-btn" disabled={busy} onClick={startLogin}>
                <RefreshCw size={14} /> Re-link <ExternalLink size={12} />
              </button>
              <button className="btn danger" disabled={busy} onClick={() => setUnlinkOpen(true)}>
                <Unlink size={14} /> Unlink…
              </button>
            </div>
          </>
        ) : (
          <>
            <p>
              Link your DegenBox Discord account once. It authorizes both the Solana and
              the Perpetuals side of this device. Connecting opens your browser for the
              normal Discord login, then hands a token back to this app. No password ever
              touches this machine.
            </p>
            {discord?.pending && (
              <div className="banner info" role="status">
                <span style={{ flex: 1 }}>
                  Waiting for the browser… finish the Discord authorization there. If you
                  closed or canceled it, just start again.
                </span>
              </div>
            )}
            {(err ?? discord?.error) && <div className="error-box">{err ?? discord?.error}</div>}
            <div className="btn-row">
              <button className="btn discord-btn" disabled={busy} onClick={startLogin}>
                {discord?.pending ? "Restart login" : "Connect Discord"}{" "}
                <ExternalLink size={12} />
              </button>
            </div>
          </>
        )}
      </div>

      <DangerConfirm
        open={unlinkOpen}
        title="Unlink Discord"
        phrase="unlink"
        busy={busy}
        error={err}
        onCancel={() => setUnlinkOpen(false)}
        onConfirm={unlink}
      >
        <p>
          This removes the stored token from this device (the desktop logout). Solana
          reads fall back to a pairing token or a web-app session if present; otherwise
          they stop until you link again. Your Discord account itself is untouched.
        </p>
      </DangerConfirm>
    </>
  );
}
