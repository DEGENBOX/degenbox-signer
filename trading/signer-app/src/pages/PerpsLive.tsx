// Perpetuals → LIVE (slice-2 IA). The module home: the unified status
// line (Gateway · Engine · Signing · Mode · Heartbeat · Last activity —
// same wording/order as Solana, spec §A; Mode is the one genuinely
// venue-specific item), the open positions grouped under the sole
// executor, and the live activity feed folded in collapsed.
//
// Positions + close/TP-SL controls are the existing, battle-tested
// PerpsPositions body, rendered embedded (its own <h1> suppressed).

import { useEffect, useState } from "react";
import { FlaskConical, Zap } from "lucide-react";
import type { HlStatus, StatusReport } from "../ipc";
import { ipc } from "../ipc";
import { commands } from "../lib/commands";
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
import { PerpsPositions } from "./PerpsPositions";
import { PerpsActivityFeed } from "../features/activity/ActivityFeed";

interface Props {
  status: StatusReport | null;
  hl: HlStatus | null;
  onReload: () => void;
  onGoBots: () => void;
}

function engineMeta(conn: HlStatus["conn"] | undefined): {
  dot: "green" | "amber" | "red" | "grey";
  pulse: boolean;
  label: string;
  title?: string;
} {
  switch (conn) {
    case "ready":
      return {
        dot: "green",
        pulse: true,
        label: "Running",
        title: "The Perpetuals engine is live and polling for work",
      };
    case "connecting":
      return { dot: "amber", pulse: true, label: "Connecting" };
    case "paused":
      return { dot: "amber", pulse: false, label: "Paused" };
    case "error":
      return { dot: "red", pulse: false, label: "Error" };
    default:
      return {
        dot: "grey",
        pulse: false,
        label: "Not running",
        title: "No Hyperliquid wallet is unlocked, so the engine has nothing to run",
      };
  }
}

export function PerpsLive({ status, hl, onReload, onGoBots }: Props) {
  const [heartbeat, setHeartbeat] = useState<string | null>(null);
  const gateway = useGatewayLink();
  const lastSignAt = useLastSignAt("hl");

  // The pairing heartbeat is the server-side proof this signer lives.
  useEffect(() => {
    let alive = true;
    const load = () =>
      ipc.hlPairingStatus().then(
        (p) => alive && setHeartbeat(p?.last_heartbeat_at ?? null),
        () => {},
      );
    load();
    const id = setInterval(load, 15_000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  const eng = engineMeta(hl?.conn);
  const paused = status?.paused ?? false;
  const paper = hl?.paper_mode ?? false;
  const canGoLive = hl?.paired ?? false;

  const items: StatusItem[] = [
    gatewayItem(gateway),
    {
      label: "Engine",
      value: eng.label,
      dot: eng.dot,
      pulse: eng.pulse,
      title: eng.title ?? hl?.error ?? undefined,
    },
    signingItem(paused),
    {
      label: "Mode",
      value: paper ? "Paper" : "Live",
      dot: paper ? "amber" : "green",
      title: paper
        ? "Paper: orders resolve and report but never reach the exchange"
        : "Live: orders go to the exchange",
    },
    heartbeatItem(
      heartbeat ?? hl?.last_poll_at ?? null,
      "Last heartbeat the gateway saw from this signer",
    ),
    lastActivityItem(
      lastSignAt ?? hl?.last_poll_at ?? null,
      "Most recent order this device signed on Perpetuals",
    ),
  ];

  return (
    <>
      <StatusLine
        items={items}
        right={<PaperToggle paper={paper} canGoLive={canGoLive} onReload={onReload} />}
      />

      <PerpsPositions hl={hl} onReload={onReload} onGoOverview={onGoBots} embedded />

      <Collapsible title="Live activity" defaultOpen={false}>
        <PerpsActivityFeed />
      </Collapsible>
    </>
  );
}

function PaperToggle({
  paper,
  canGoLive,
  onReload,
}: {
  paper: boolean;
  canGoLive: boolean;
  onReload: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const goLiveBlocked = paper && !canGoLive;
  const toggle = async () => {
    setBusy(true);
    try {
      await commands.perps.setPaperMode(!paper);
      onReload();
    } finally {
      setBusy(false);
    }
  };
  return (
    <button
      className={`btn sm ${paper ? "" : "paused-state"}`}
      disabled={busy || goLiveBlocked}
      onClick={toggle}
      title={
        goLiveBlocked
          ? "Pair this device on the Bots tab before going live"
          : paper
            ? "Back to live: orders reach the exchange again"
            : "Switch to paper: new orders simulate only"
      }
    >
      {paper ? <Zap size={12} /> : <FlaskConical size={12} />}
      {paper ? "Go live" : "Go paper"}
    </button>
  );
}
