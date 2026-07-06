// Solana → LIVE (slice-2 IA). The module home: the unified status line
// (Gateway · Engine · Signing · Heartbeat · Last activity — same
// wording/order as Perpetuals, spec §A), the compact per-bot positions
// table and the live activity feed folded in collapsed.
//
// Note: unlike Perpetuals there is no signer-side paper/live toggle for
// Solana (the venue live flag is server-side, sol_user_trading); the
// device-wide signing kill-switch is the local control, shown here.

import { useEffect, useState } from "react";
import { ArrowRight } from "lucide-react";
import type {
  BotPreset,
  SolCopyConfigFull,
  SolRuntimeStatus,
  StatusReport,
} from "../ipc";
import { ipc } from "../ipc";
import { summarizeRunning } from "../features/bots/running";
import { StatusLine, type StatusItem } from "../components/StatusLine";
import {
  gatewayItem,
  heartbeatItem,
  lastActivityItem,
  signingItem,
  useGatewayLink,
  useLastSignAt,
} from "../components/statusItems";
import { Collapsible } from "../components/Collapsible";
import { SolPositions } from "./SolPositions";
import { SolActivityFeed } from "../features/activity/ActivityFeed";

function engineMeta(state: SolRuntimeStatus["state"] | undefined): {
  dot: "green" | "amber" | "red" | "grey";
  pulse: boolean;
  label: string;
  title?: string;
} {
  switch (state) {
    case "ready":
      return {
        dot: "green",
        pulse: true,
        label: "Running",
        title: "The Solana engine is live on both trade streams",
      };
    case "connecting":
      return { dot: "amber", pulse: true, label: "Connecting" };
    case "waiting_auth":
      return {
        dot: "amber",
        pulse: false,
        label: "Waiting for sign-in",
        title: "The engine idles until this device holds gateway credentials",
      };
    case "auth_expired":
      return {
        dot: "red",
        pulse: false,
        label: "Session expired",
        title: "The gateway rejected this device. Re-link from the account menu",
      };
    case "error":
      return { dot: "red", pulse: false, label: "Error" };
    default:
      return {
        dot: "grey",
        pulse: false,
        label: "Not running",
        title: "No Solana wallet is unlocked, so the engine has nothing to run",
      };
  }
}

export function SolLive({
  status,
  onGoBots,
}: {
  status: StatusReport | null;
  onGoBots: () => void;
}) {
  const [runtime, setRuntime] = useState<SolRuntimeStatus | null>(null);
  // Running-now strip data (R4): the same sessions/follows/armed truth
  // the Bots tab polls, folded into this tab's existing 10 s cadence.
  const [botSessions, setBotSessions] = useState<BotPreset[] | null>(null);
  const [follows, setFollows] = useState<SolCopyConfigFull[] | null>(null);
  const [armedIds, setArmedIds] = useState<Set<string> | null>(null);
  const gateway = useGatewayLink();
  const lastSignAt = useLastSignAt("sol");

  useEffect(() => {
    let alive = true;
    const load = async () => {
      const [rt, se, fo, dev] = await Promise.allSettled([
        ipc.solRuntimeStatus(),
        ipc.botPresets(),
        ipc.solCopyConfigsFull(),
        ipc.botDeviceStatus(),
      ]);
      if (!alive) return;
      if (rt.status === "fulfilled") setRuntime(rt.value);
      if (se.status === "fulfilled") setBotSessions(se.value);
      if (fo.status === "fulfilled") setFollows(fo.value);
      if (dev.status === "fulfilled") {
        setArmedIds(new Set(dev.value.armed_session_ids));
      }
    };
    load();
    const id = setInterval(load, 10_000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  const eng = engineMeta(runtime?.state);
  const paused = status?.paused ?? false;
  const lastActivity = runtime?.last_event_at ?? lastSignAt;

  const items: StatusItem[] = [
    gatewayItem(gateway),
    {
      label: "Engine",
      value: eng.label,
      dot: eng.dot,
      pulse: eng.pulse,
      title: eng.title ?? runtime?.error ?? undefined,
    },
    signingItem(paused),
    heartbeatItem(
      runtime?.alive_at ?? null,
      "Last proof of life from the Solana engine (stamped every 30 s while it runs)",
    ),
    lastActivityItem(
      lastActivity,
      "Most recent buy/sell event this device handled on Solana",
    ),
  ];

  const run = summarizeRunning(botSessions, follows, armedIds);

  return (
    <>
      <StatusLine items={items} />
      {/* One-line "what's live" strip — counts only, names on hover,
          click lands on the Bots tab. Positions stay the main event. */}
      <button
        type="button"
        className="run-strip"
        onClick={onGoBots}
        title={run.title || "Nothing is live. Set up auto-buy or copy trade on the Bots tab"}
      >
        <span
          className={`status-dot ${run.dot === "grey" ? "" : run.dot} ${
            run.pulse ? "pulse" : ""
          }`}
        />
        <span className="hud-label">Running now</span>
        <span className="run-strip-text">{run.line}</span>
        {run.unarmed > 0 && (
          <span
            className="badge warn"
            title="Live on the server but not armed on this device, so no auto-buy fires from here"
          >
            {run.unarmed === 1
              ? "1 not armed on this device"
              : `${run.unarmed} not armed on this device`}
          </span>
        )}
        <span className="run-strip-go">
          Bots tab <ArrowRight size={12} />
        </span>
      </button>
      <SolPositions embedded />
      <Collapsible title="Live activity" defaultOpen={false}>
        <SolActivityFeed />
      </Collapsible>
    </>
  );
}
