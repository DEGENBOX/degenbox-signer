// Pure display helpers for the Perpetuals Bots tab — the perps
// counterpart of features/bots/meta.ts. Maps the HL daemon's conn
// states and the gateway's pairing states onto the shared dot/pill
// vocabulary so the executor card speaks the same idiom as the Sol
// ClientCards.

import type { StatusPillTone } from "@degenbox/ui";
import type { HlStatus } from "./ipc";

export type Dot = "green" | "amber" | "red" | "grey";

export interface ConnMeta {
  dot: Dot;
  /** Short status word for the hud line. */
  label: string;
  /** Pulse ring on the dot — only a live executor breathes. */
  pulse: boolean;
}

/** HL daemon conn → dot + label ("ready" reads as "executing" to match
 * the Sol runtime vocabulary — a ready daemon is polling the queue). */
const CONN_META: Record<string, ConnMeta> = {
  offline: { dot: "red", label: "offline", pulse: false },
  connecting: { dot: "amber", label: "connecting", pulse: false },
  ready: { dot: "green", label: "executing", pulse: true },
  paused: { dot: "amber", label: "paused", pulse: false },
  error: { dot: "red", label: "error", pulse: false },
};

export function connMeta(hl: HlStatus | null): ConnMeta {
  if (!hl) return { dot: "grey", label: "…", pulse: false };
  return CONN_META[hl.conn] ?? { dot: "grey", label: hl.conn, pulse: false };
}

/** Server-side pairing states → pill tone + human label (the gateway
 * can disagree with the local "paired" flag — its word wins). */
export const PAIRING_PILL: Record<string, { tone: StatusPillTone; label: string }> = {
  paired_live: { tone: "ok", label: "paired · live" },
  paired_offline: { tone: "warn", label: "paired · offline" },
  unpaired: { tone: "danger", label: "unpaired on server" },
  wallet_mismatch: { tone: "danger", label: "wallet mismatch" },
  pending_approval: { tone: "warn", label: "pending approval" },
  revoked: { tone: "danger", label: "revoked" },
  not_registered: { tone: "danger", label: "not registered" },
};

/** A pairing state the executor can actually deliver trades in. */
export function pairingHealthy(state: string): boolean {
  return state === "paired_live" || state === "paired_offline";
}
