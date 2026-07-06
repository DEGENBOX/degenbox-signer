// Root component — W1 shell (bot-redesign 2026-06).
//
// IA: a top header carries the brand, the GLOBAL Solana/Perpetuals
// mode switch (flips the accent/glow token family via styles/mode.ts,
// persisted) and the operationally-critical daemon health + pause
// kill-switch (formerly sidebar foot). Under it, four tabs — Account ·
// Positions · Presets · Bots — identical in both modes; routing is a
// (mode × tab) matrix. "Hyperliquid" is labeled "Perpetuals"
// everywhere in the new shell (code ids stay `hl`).
//
// The pre-redesign sidebar pages (Home/Clients, SolOverview,
// HlOverview, HlPositions, Copytrade, Account, Settings) were replaced
// by the tab surfaces above and deleted in W5.
//
// Unlock-UX (locked decision: unlock once per start, holds until
// app-close or access loss):
//   · vault exists + locked  → full-screen Unlock view (pages/Unlock),
//     never buried in Settings. The Rust boot keychain auto-unlock
//     usually beats the first status poll, skipping the screen.
//   · NO idle/timeout re-lock exists (audited) — lock only via the
//     Account tab's explicit button, app close, or access loss.
//   · access loss: every 5 min `access_check` probes `GET /auth/me`
//     with the resolved credentials; the Sol runtime's `auth_expired`
//     flag (a live 401/403 from gateway calls) is polled every 15 s as
//     the fast path — but the flag alone never locks: an authoritative
//     `access_check` "revoked" verdict is required (an EXPIRED token
//     is re-login, never local-key loss — audit 2026-06-12 N3). On
//     loss → lock_keystores + Unlock view with an "ACCESS REVOKED /
//     SUB EXPIRED" reason. Edge-triggered: a manual re-unlock is
//     respected until the gateway answers ok again (prevents a hostile
//     lock loop while the user re-links).

import { useCallback, useEffect, useRef, useState } from "react";
import { Pause, Play, ShieldCheck } from "lucide-react";
import { ipc, type HlStatus, type StatusReport } from "./ipc";
import { getMode, setMode, type Mode } from "./styles/mode";
import { Onboarding } from "./pages/Onboarding";
import { Unlock } from "./pages/Unlock";
import { SolLive } from "./pages/SolLive";
import { PerpsLive } from "./pages/PerpsLive";
import { SolBotsTab } from "./pages/SolBotsTab";
import { PerpsBotsTab } from "./pages/PerpsBotsTab";
import { AccountMenu } from "./components/AccountMenu";
import { EmergencyFlatten } from "./components/EmergencyFlatten";
import { ErrorBoundary } from "./components/ErrorBoundary";

// Slice-2 IA: per module exactly two tabs — LIVE (the home: status +
// positions-per-bot + folded activity) and BOTS (create + library +
// presets). Account left the modules → the top-right avatar overlay.
type Tab = "live" | "bots";

const TABS: { id: Tab; label: string; num: string; key: string }[] = [
  { id: "live", label: "Live", num: "01", key: "1" },
  { id: "bots", label: "Bots", num: "02", key: "2" },
];

const TAB_KEY = "degenbox.signer.tab";

function loadTab(): Tab {
  try {
    const t = localStorage.getItem(TAB_KEY);
    if (t === "live" || t === "bots") return t;
  } catch {
    // storage unavailable — default below
  }
  return "live";
}

const ACCESS_REASON = "Access revoked / sub expired";

export function App() {
  const [mode, setModeState] = useState<Mode>(() => getMode());
  const [tab, setTabState] = useState<Tab>(() => loadTab());
  const [needsOnboarding, setNeedsOnboarding] = useState<boolean | null>(null);
  const [status, setStatus] = useState<StatusReport | null>(null);
  const [hl, setHl] = useState<HlStatus | null>(null);
  const [pauseBusy, setPauseBusy] = useState(false);
  const [version, setVersion] = useState("");
  const [flattenNotice, setFlattenNotice] = useState<string | null>(null);
  // Access-loss lock state. `accessLost` drives the unlock-screen
  // reason + the in-shell warning after a conscious re-unlock; the ref
  // is the edge-trigger guard (one lock per loss episode).
  const [accessLost, setAccessLost] = useState(false);
  const [lockReason, setLockReason] = useState<string | null>(null);
  const [lockDetail, setLockDetail] = useState<string | null>(null);
  const accessLostRef = useRef(false);

  const setTab = useCallback((t: Tab) => {
    setTabState(t);
    try {
      localStorage.setItem(TAB_KEY, t);
    } catch {
      // session-only
    }
  }, []);

  const switchMode = useCallback((m: Mode) => {
    setMode(m); // persists + flips .mode-sol/.mode-perps on <html>
    setModeState(m);
  }, []);

  useEffect(() => {
    ipc.appVersion().then(setVersion).catch(() => {});
  }, []);

  const reload = useCallback(async () => {
    try {
      setStatus(await ipc.status());
    } catch {
      // stays null — header pill shows red
    }
    try {
      setHl(await ipc.hlStatus());
    } catch {
      // keep last snapshot
    }
  }, []);

  useEffect(() => {
    (async () => {
      try {
        const ob = await ipc.onboardingState();
        setNeedsOnboarding(ob.needs_onboarding);
      } catch {
        setNeedsOnboarding(false);
      }
      reload();
    })();
    const id = setInterval(reload, 2000);
    return () => clearInterval(id);
  }, [reload]);

  // ─── access-loss watcher (W1 unlock-UX) ─────────────────────────
  const onAccessLoss = useCallback(
    (detail: string | null) => {
      if (accessLostRef.current) return; // edge-triggered
      accessLostRef.current = true;
      setAccessLost(true);
      setLockReason(ACCESS_REASON);
      setLockDetail(detail);
      // lock_keystores stops every runtime, wipes decrypted secrets
      // AND drops the cached keychain passphrase, so a relaunch stays
      // locked too. The next status poll flips the UI to the gate.
      ipc.lock().then(reload, () => reload());
    },
    [reload],
  );

  // Slow path: gateway /auth/me probe every 5 minutes (also runs while
  // locked, so a restored sub clears the episode before re-unlock).
  useEffect(() => {
    let alive = true;
    const probe = async () => {
      try {
        const r = await ipc.accessCheck();
        if (!alive) return;
        if (r.state === "ok") {
          accessLostRef.current = false;
          setAccessLost(false);
        } else if (r.state === "revoked") {
          onAccessLoss(r.detail);
        }
        // "no_auth" (never linked) / "unreachable" (network, 5xx):
        // never lock — only an authoritative 401/403 is access loss.
      } catch {
        // IPC unavailable — watcher idles, never locks.
      }
    };
    probe();
    const id = setInterval(probe, 5 * 60 * 1000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [onAccessLoss]);

  // Fast path: the Sol runtime flips `auth_expired` the moment a live
  // gateway call answers 401/403 (stream upgrade, relay REST). That
  // flag alone is NOT authoritative (audit 2026-06-12 N3): a token
  // that simply EXPIRED while the laptop slept — or a stray
  // third-party 401 leaking into an engine error — must mean re-login,
  // never a vault lock + keychain-passphrase drop. Confirm with the
  // same `access_check` probe the slow path trusts; ONLY its `revoked`
  // verdict (a live gateway 401/403 that is NOT an ExpiredSignature)
  // locks. "ok" / "no_auth" / "unreachable" never lock.
  useEffect(() => {
    let alive = true;
    let confirming = false;
    const tick = async () => {
      if (confirming) return; // one in-flight probe at a time
      try {
        const rs = await ipc.solRuntimeStatus();
        if (!alive || rs.state !== "auth_expired") return;
        confirming = true;
        try {
          const check = await ipc.accessCheck();
          if (alive && check.state === "revoked") {
            onAccessLoss(
              check.detail ??
                rs.error ??
                "gateway rejected this device's credentials (401)",
            );
          }
        } finally {
          confirming = false;
        }
      } catch {
        // runtime not up (locked / Solana not set up) — nothing to observe
      }
    };
    tick();
    const id = setInterval(tick, 15000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [onAccessLoss]);

  // Keyboard nav: 1-4 jump between tabs, M toggles the mode (ignored
  // while typing).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) {
        return;
      }
      const hit = TABS.find((n) => n.key === e.key);
      if (hit) setTab(hit.id);
      if (e.key === "m" || e.key === "M") {
        switchMode(mode === "sol" ? "perps" : "sol");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [mode, setTab, switchMode]);

  if (needsOnboarding === null) {
    return <div className="shell" />;
  }

  // First-run (no vault yet) — reachable BEFORE any unlock gate.
  if (needsOnboarding) {
    return (
      <Onboarding
        onDone={() => {
          setNeedsOnboarding(false);
          reload();
        }}
      />
    );
  }

  // Unlock gate: vault exists (addresses are vault metadata, readable
  // while locked) but no chain is unlocked → full-screen unlock. The
  // boot keychain auto-unlock usually lands before the first poll.
  const vaultExists = !!(status?.hl_address || status?.sol_pubkey);
  const anyUnlocked = !!(status?.hl_unlocked || status?.sol_unlocked);
  if (status && vaultExists && !anyUnlocked) {
    return (
      <Unlock
        reason={accessLost ? lockReason : null}
        reasonDetail={accessLost ? lockDetail : null}
        onUnlocked={() => {
          setLockReason(null);
          setLockDetail(null);
          reload();
        }}
      />
    );
  }

  const paused = status?.paused ?? false;
  const healthLabel = !status
    ? "Connecting"
    : paused
      ? "Paused"
      : status.health === "green"
        ? "Online"
        : status.health === "amber"
          ? "Degraded"
          : "Locked";

  const togglePause = async () => {
    if (!status) return;
    setPauseBusy(true);
    try {
      await ipc.setPaused(!paused);
      await reload();
    } catch {
      // next poll reconciles
    } finally {
      setPauseBusy(false);
    }
  };

  return (
    <div className="shell">
      <header className="topbar">
        <div className="brand">
          <img src="/degenbox-logo.png" alt="" aria-hidden />
          <div>
            <div className="name">DegenBox</div>
            <div className="sub">Trading client</div>
          </div>
        </div>

        <div className="mode-switch" role="tablist" aria-label="Market mode">
          <button
            role="tab"
            aria-selected={mode === "sol"}
            className={mode === "sol" ? "active corners" : ""}
            title="Solana mode (M toggles)"
            onClick={() => switchMode("sol")}
          >
            <span className="mode-dot" /> Solana
          </button>
          <button
            role="tab"
            aria-selected={mode === "perps"}
            className={mode === "perps" ? "active corners" : ""}
            title="Perpetuals mode (M toggles)"
            onClick={() => switchMode("perps")}
          >
            <span className="mode-dot" /> Perpetuals
          </button>
        </div>

        <div className="topbar-right">
          {/* Module-scoped money kill — flattens the ACTIVE venue. */}
          <EmergencyFlatten mode={mode} onDone={setFlattenNotice} />
          {/* Colored like the flatten button (bordered tint), no dot —
              the pill ITSELF carries the state color. */}
          <div
            className={`health-pill ${
              !status
                ? "state-amber" /* Connecting = amber, not an error */
                : paused
                  ? "state-amber"
                  : status.health === "green"
                    ? "state-green"
                    : status.health === "amber"
                      ? "state-amber"
                      : "state-red"
            }`}
            title={`Signer daemon: ${healthLabel}`}
          >
            {healthLabel}
            {hl?.paper_mode && <span className="badge warn">paper</span>}
          </div>
          <button
            className={`btn ${paused ? "paused-state" : ""}`}
            disabled={pauseBusy || !status}
            onClick={togglePause}
            title={
              paused
                ? "Resume signing on this device (both chains)"
                : "Pause all signing on this device (both chains). Queued orders wait"
            }
          >
            {paused ? (
              <>
                <Play size={13} /> Resume
              </>
            ) : (
              <>
                <Pause size={13} /> Pause
              </>
            )}
          </button>
          <span className="hud-label" title="App version">
            {version ? `v${version}` : ""}
          </span>
          <AccountMenu status={status} onReload={reload} />
        </div>
      </header>

      <nav className="tabbar" aria-label="Sections">
        {TABS.map(({ id, label, num, key }) => (
          <button
            key={id}
            className={`tab-item ${tab === id ? "active" : ""}`}
            aria-current={tab === id ? "page" : undefined}
            title={`Press ${key}`}
            onClick={() => setTab(id)}
          >
            <span className="tab-num">{num}</span> {label}
          </button>
        ))}
        <span className="tabbar-mode hud-label brackets">
          {mode === "sol" ? "Sol" : "Perps"}
        </span>
      </nav>

      <main className="shell-main">
        <div className="container">
          {hl?.totp_prompt && <TotpBanner onSubmitted={reload} />}
          {accessLost && (
            <div className="banner warn" role="alert">
              <ShieldCheck size={16} style={{ flexShrink: 0 }} />
              <span style={{ flex: 1 }}>
                <strong>Access revoked / subscription expired</strong>: the gateway
                rejected this device's credentials. Signing stays off until access is
                restored. Re-link from the account menu (top right).
              </span>
            </div>
          )}
          {flattenNotice && (
            <div className="banner" role="status">
              <span style={{ flex: 1 }}>{flattenNotice}</span>
              <button className="btn" onClick={() => setFlattenNotice(null)}>
                Dismiss
              </button>
            </div>
          )}
          {/* keyed pane = 300ms entrance on tab/mode switch (calm pass).
              A per-tab ErrorBoundary (keyed the same) isolates a crash in
              one surface so the header / mode switch / kill-switch /
              Account and the other tab stay usable. */}
          <ErrorBoundary
            key={`${mode}-${tab}`}
            label={`${mode === "sol" ? "Solana" : "Perpetuals"} · ${tab === "live" ? "Live" : "Bots"}`}
          >
            <div className="tab-pane">
              {tab === "live" &&
                (mode === "sol" ? (
                  <SolLive status={status} onGoBots={() => setTab("bots")} />
                ) : (
                  <PerpsLive
                    status={status}
                    hl={hl}
                    onReload={reload}
                    onGoBots={() => setTab("bots")}
                  />
                ))}
              {tab === "bots" &&
                (mode === "sol" ? (
                  <SolBotsTab status={status} />
                ) : (
                  <PerpsBotsTab status={status} hl={hl} onReload={reload} />
                ))}
            </div>
          </ErrorBoundary>
        </div>
      </main>
    </div>
  );
}

// Global per-trade 2FA banner — visible regardless of tab so a held
// trade never goes unnoticed.
function TotpBanner({ onSubmitted }: { onSubmitted: () => void }) {
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const submit = async () => {
    if (code.length < 6) return;
    setBusy(true);
    setErr(null);
    try {
      await ipc.submitHlTotp(code);
      setCode("");
      onSubmitted();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="banner warn" role="alert">
      <ShieldCheck size={16} style={{ flexShrink: 0 }} />
      <span style={{ flex: 1 }}>
        <strong>2FA required</strong>: DegenBox is holding a trade until you confirm with
        your authenticator.
        {err && (
          <span style={{ color: "var(--red)", display: "block", fontSize: 12 }}>{err}</span>
        )}
      </span>
      <input
        className="input mono"
        style={{ width: 110 }}
        value={code}
        inputMode="numeric"
        maxLength={6}
        aria-label="Authenticator code"
        onChange={(e) => setCode(e.target.value.replace(/\D/g, ""))}
        onKeyDown={(e) => {
          if (e.key === "Enter") submit();
        }}
        placeholder="123456"
      />
      <button className="btn primary" disabled={busy || code.length < 6} onClick={submit}>
        Confirm
      </button>
    </div>
  );
}
