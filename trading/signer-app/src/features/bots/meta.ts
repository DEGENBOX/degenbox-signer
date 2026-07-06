// Pure display/grouping helpers for the Bots tab. Logic moved (and
// Sol-narrowed) from components/clientMeta.ts so the legacy fleet
// files (Home/ClientTable/clientMeta) stay deletable in W5.

import type { BotPreset, ClientInfo } from "./ipc";

export type Dot = "green" | "amber" | "red" | "grey";

export interface RuntimeMeta {
  dot: Dot;
  /** Short uppercase-able status word for the hud line. */
  label: string;
  /** Pulse ring on the dot — only a live executor breathes. */
  pulse: boolean;
  detail: string | null;
}

/** Map `runtime_state` strings (executor:ready | standby[:…] | locked |
 * remote) to a dot + label. Paused wins over everything. */
export function runtimeMeta(c: ClientInfo): RuntimeMeta {
  const detail = c.runtime_detail;
  if (c.paused) return { dot: "amber", label: "paused", pulse: false, detail };
  const s = c.runtime_state;
  if (s.startsWith("executor")) {
    if (s.includes("ready")) return { dot: "green", label: "executing", pulse: true, detail };
    if (s.includes("auth_expired")) {
      return { dot: "red", label: "re-login required", pulse: false, detail };
    }
    if (s.includes("offline")) {
      return { dot: "red", label: "executor offline", pulse: false, detail };
    }
    const rest = s.slice("executor".length).replace(/^:/, "");
    return {
      dot: "amber",
      label: rest ? `executor · ${rest}` : "executor",
      pulse: false,
      detail,
    };
  }
  if (s.startsWith("standby")) {
    const registered = s.includes("registered");
    return { dot: registered ? "green" : "grey", label: "standby", pulse: false, detail };
  }
  if (s === "locked") return { dot: "red", label: "locked", pulse: false, detail };
  if (s === "remote") return { dot: "grey", label: "remote", pulse: false, detail };
  return { dot: "grey", label: s || "unknown", pulse: false, detail };
}

/** Lenient numeric parse for gateway `unknown` fields. */
export function num(v: unknown): number | null {
  if (typeof v === "number") return Number.isFinite(v) ? v : null;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  }
  return null;
}

export const isRemote = (c: ClientInfo) => c.id.startsWith("gw-");

/** Solana slice of the fleet (this tab never shows HL wallets —
 * Perpetuals has its own Bots surface). */
export function solClients(list: ClientInfo[]): ClientInfo[] {
  return list.filter((c) => c.chain === "sol");
}

/** Stable ordering: local before remote, primary on top, then
 * label/address. */
export function sortClients(list: ClientInfo[]): ClientInfo[] {
  return [...list].sort((a, b) => {
    const ra = isRemote(a) ? 1 : 0;
    const rb = isRemote(b) ? 1 : 0;
    if (ra !== rb) return ra - rb;
    if (a.primary !== b.primary) return a.primary ? -1 : 1;
    const la = (a.label ?? a.address).toLowerCase();
    const lb = (b.label ?? b.address).toLowerCase();
    return la.localeCompare(lb);
  });
}

export interface GroupedSessions {
  /** client.address → its sessions (server rows carry wallet_pubkey). */
  byWallet: Map<string, BotPreset[]>;
  /** Sessions whose wallet matches no visible client (other device /
   * removed wallet) — still controllable, never hidden. */
  unbound: BotPreset[];
}

export function groupSessions(
  sessions: BotPreset[] | null,
  clients: ClientInfo[],
): GroupedSessions {
  const byWallet = new Map<string, BotPreset[]>();
  const unbound: BotPreset[] = [];
  const addrs = new Set(clients.map((c) => c.address));
  for (const s of sessions ?? []) {
    if (s.wallet_pubkey && addrs.has(s.wallet_pubkey)) {
      const arr = byWallet.get(s.wallet_pubkey) ?? [];
      arr.push(s);
      byWallet.set(s.wallet_pubkey, arr);
    } else {
      unbound.push(s);
    }
  }
  return { byWallet, unbound };
}

/** "in 3h" / "in 2d" for a future timestamp (session expiry). */
export function fmtIn(iso: string): string {
  const ms = new Date(iso).getTime() - Date.now();
  if (!Number.isFinite(ms) || ms <= 0) return "now";
  const m = Math.floor(ms / 60_000);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}
