// Shared status-line data for BOTH module LIVE tabs (slice 9, spec §A).
// One source for wording, order and states so Solana and Perpetuals
// read identically — only genuinely venue-specific bits differ:
//
//   Gateway · Engine · Signing · [Mode, perps only] · Heartbeat · Last activity
//
//  * Gateway — the real gateway link (an `access_check` probe against
//    /auth/me), NOT the venue engine. Fixes the §A bug where the line
//    said "offline" although the gateway was fine.
//  * Engine — the venue executor (Sol dispatcher / perps daemon).
//  * Signing — the device-wide kill switch.
//  * Heartbeat — proof of life: Sol = the engine's own 30 s liveness
//    stamp; perps = the pairing heartbeat the server last saw.
//  * Last activity — the venue's most recent handled event / signature.

import { useEffect, useState } from "react";
import { ipc, type AccessCheck, type RecentSign } from "../ipc";
import { timeAgo } from "./ui";
import type { DotTone, StatusItem } from "./StatusLine";

// ─── gateway link (shared probe) ────────────────────────────────────

export type GatewayLinkState = AccessCheck["state"] | "checking";

/** Poll `access_check` — the same probe the access-loss watcher trusts. */
export function useGatewayLink(pollMs = 30_000): GatewayLinkState {
  const [state, setState] = useState<GatewayLinkState>("checking");
  useEffect(() => {
    let alive = true;
    const load = () =>
      ipc.accessCheck().then(
        (r) => alive && setState(r.state),
        () => alive && setState("unreachable"),
      );
    load();
    const id = setInterval(load, pollMs);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [pollMs]);
  return state;
}

export function gatewayItem(state: GatewayLinkState): StatusItem {
  const map: Record<string, { dot: DotTone; pulse: boolean; value: string; title: string }> = {
    checking: {
      dot: "grey",
      pulse: false,
      value: "Checking…",
      title: "Probing the DegenBox gateway",
    },
    ok: {
      dot: "green",
      pulse: true,
      value: "Connected",
      title: "The gateway accepts this device's credentials",
    },
    no_auth: {
      dot: "amber",
      pulse: false,
      value: "Not linked",
      title: "No DegenBox account linked yet. Sign in from the account menu (top right)",
    },
    revoked: {
      dot: "red",
      pulse: false,
      value: "Access revoked",
      title: "The gateway rejected this device. Re-link your account",
    },
    unreachable: {
      dot: "red",
      pulse: false,
      value: "Unreachable",
      title: "Can't reach the gateway. Check your connection",
    },
  };
  const m = map[state] ?? map.unreachable;
  return { label: "Gateway", value: m.value, dot: m.dot, pulse: m.pulse, title: m.title };
}

// ─── signing (device kill switch) ───────────────────────────────────

export function signingItem(paused: boolean): StatusItem {
  return {
    label: "Signing",
    value: paused ? "Paused" : "Active",
    dot: paused ? "amber" : "green",
    title: paused
      ? "The device-wide pause is on. Nothing buys or sells from here"
      : "This device signs and executes",
  };
}

// ─── heartbeat + last activity ──────────────────────────────────────

export function heartbeatItem(iso: string | null, title: string): StatusItem {
  return {
    label: "Heartbeat",
    value: iso ? timeAgo(iso) : "—",
    title: iso ? title : `${title}: none yet`,
  };
}

export function lastActivityItem(iso: string | null, title: string): StatusItem {
  return {
    label: "Last activity",
    value: iso ? timeAgo(iso) : "—",
    title: iso ? title : `${title}: nothing handled yet`,
  };
}

/** Newest recent-sign timestamp for one venue — the venue's real "last
 * did something" (the ring covers this app run). */
export function useLastSignAt(chain: "sol" | "hl", pollMs = 15_000): string | null {
  const [at, setAt] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    const load = () =>
      ipc.recentSigns().then(
        (rows: RecentSign[]) => {
          if (!alive) return;
          const hit = rows.find((r) => r.chain === chain);
          setAt(hit?.at ?? null);
        },
        () => {},
      );
    load();
    const id = setInterval(load, pollMs);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [chain, pollMs]);
  return at;
}
