// ClientsStrip — one compact corner-bracketed card per Solana client
// over the positions table: name, wallet, assigned presets/copy
// targets, live SOL balance, attributed unrealized PnL and the W2.1
// daily/weekly/monthly realized-PnL windows. A global sum rail closes
// the strip — THE one glow focus of this screen.
//
// Data ownership: the parent (SolPositions) owns clients + positions +
// the unit toggle; this component owns the strip-only feeds (PnL
// windows, per-wallet balances, per-client assignment names) so a
// strip hiccup never blanks the table.

import { useEffect, useMemo, useState } from "react";
import { Users } from "lucide-react";
import { ipc, type ClientInfo, type SolWalletBalance } from "../../ipc";
import {
  fetchPnlWindows,
  fmtSolAmt,
  lamportsToSol,
  num,
  type PnlWindowsResponse,
} from "./data";
import { EmptyState, fmtUsd, shortAddr } from "../../components/ui";

export type Unit = "sol" | "usd";

export interface UpnlSums {
  usd: number | null;
  sol: number | null;
}

interface Assignments {
  presets: string[];
  copies: string[];
}

interface Props {
  /** Solana clients (local + remote), parent-owned. null = loading. */
  clients: ClientInfo[] | null;
  /** Attributed unrealized PnL per GATEWAY client id. */
  upnlByGw: Map<string, UpnlSums>;
  /** Position-table totals (uPnL across all positions). */
  upnlTotal: UpnlSums;
  unit: Unit;
}

const WINDOWS_POLL_MS = 30_000;
const BALANCE_POLL_MS = 30_000;
const ASSIGN_POLL_MS = 60_000;

function signed(v: number | null, unit: Unit): string {
  if (v == null) return "—";
  const sign = v > 0 ? "+" : "";
  return unit === "usd" ? `${sign}${fmtUsd(String(v))}` : `${sign}${fmtSolAmt(v)}`;
}

function toneCls(v: number | null): string {
  if (v == null || v === 0) return "text-ink-3";
  return v > 0 ? "text-up" : "text-down";
}

export function ClientsStrip({ clients, upnlByGw, upnlTotal, unit }: Props) {
  const [windows, setWindows] = useState<PnlWindowsResponse | null>(null);
  const [balances, setBalances] = useState<Record<string, SolWalletBalance>>({});
  const [assignments, setAssignments] = useState<Record<string, Assignments>>({});

  // Realized-PnL windows (gateway, W2.1).
  useEffect(() => {
    let alive = true;
    const load = () =>
      fetchPnlWindows().then(
        (w) => {
          if (alive) setWindows(w);
        },
        () => {
          // keep the last snapshot — the strip degrades to "—"
        },
      );
    load();
    const id = setInterval(load, WINDOWS_POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  // Per-wallet SOL balances (RPC-side, works per-pubkey).
  const addrKey = (clients ?? []).map((c) => c.address).join(",");
  useEffect(() => {
    if (!addrKey) return;
    let alive = true;
    const addrs = addrKey.split(",");
    const load = async () => {
      const results = await Promise.allSettled(addrs.map((a) => ipc.solBalance(a)));
      if (!alive) return;
      const map: Record<string, SolWalletBalance> = {};
      results.forEach((r, i) => {
        if (r.status === "fulfilled") map[addrs[i]] = r.value;
      });
      if (Object.keys(map).length > 0) setBalances((prev) => ({ ...prev, ...map }));
    };
    load();
    const id = setInterval(load, BALANCE_POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [addrKey]);

  // Assigned preset / copy-target names per gateway client.
  const gwKey = (clients ?? [])
    .map((c) => c.gateway?.id)
    .filter(Boolean)
    .join(",");
  useEffect(() => {
    if (!gwKey) return;
    let alive = true;
    const ids = gwKey.split(",");
    const load = async () => {
      const results = await Promise.allSettled(
        ids.map(async (id) => {
          const [presets, copies] = await Promise.all([
            ipc.clientPresetsList(id).catch(() => []),
            ipc.clientCopyConfigs(id).catch(() => []),
          ]);
          return {
            id,
            presets: presets.filter((p) => p.enabled).map((p) => p.name),
            copies: copies
              .filter((c) => c.enabled)
              .map((c) => c.label || shortAddr(c.leader, 4, 4)),
          };
        }),
      );
      if (!alive) return;
      const map: Record<string, Assignments> = {};
      for (const r of results) {
        if (r.status === "fulfilled") {
          map[r.value.id] = { presets: r.value.presets, copies: r.value.copies };
        }
      }
      setAssignments((prev) => ({ ...prev, ...map }));
    };
    load();
    const id = setInterval(load, ASSIGN_POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [gwKey]);

  const winByGw = useMemo(() => {
    const m = new Map<string, PnlWindowsResponse["clients"][number]>();
    for (const c of windows?.clients ?? []) m.set(c.client_id, c);
    return m;
  }, [windows]);

  const list = clients ?? [];

  // ── strip totals ──────────────────────────────────────────────────
  const balTotal = (() => {
    let sum = 0;
    let any = false;
    for (const c of list) {
      const b = num(balances[c.address]?.sol_ui);
      if (b != null) {
        sum += b;
        any = true;
      }
    }
    return any ? sum : null;
  })();
  const realizedTotals = windows?.totals ?? null;
  const realizedWindow = (k: "d1" | "d7" | "d30"): number | null => {
    if (!realizedTotals) return null;
    if (unit === "sol") return lamportsToSol(realizedTotals.realized_lamports[k]);
    return realizedTotals.realized_usd ? num(realizedTotals.realized_usd[k]) : null;
  };
  const upnlSum = unit === "sol" ? upnlTotal.sol : upnlTotal.usd;

  return (
    <div className="mb-3.5">
      <div className="shell-section-head">
        <span className="section-num">01</span>
        <span className="shell-section-title">Clients</span>
        <span className="hud-label brackets">{list.length || "–"}</span>
      </div>

      {list.length === 0 ? (
        <div className="border border-line/10">
          <EmptyState
            icon={<Users size={18} />}
            title={clients === null ? "Loading clients…" : "No Solana clients yet"}
            hint={clients === null ? undefined : "add one in the Bots tab"}
          />
        </div>
      ) : (
        <div
          className="grid gap-px bg-line/10 border border-line/10"
          style={{ gridTemplateColumns: "repeat(auto-fit, minmax(230px, 1fr))" }}
        >
          {list.map((c) => {
            const gwId = c.gateway?.id ?? null;
            const win = gwId ? winByGw.get(gwId) : undefined;
            const asg = gwId ? assignments[gwId] : undefined;
            const bal = num(balances[c.address]?.sol_ui);
            const upnl = gwId ? (upnlByGw.get(gwId) ?? null) : null;
            const upnlVal = upnl == null ? null : unit === "sol" ? upnl.sol : upnl.usd;
            const w = (k: "d1" | "d7" | "d30"): number | null => {
              if (!win) return null;
              if (unit === "sol") return lamportsToSol(win.realized_lamports[k]);
              return win.realized_usd ? num(win.realized_usd[k]) : null;
            };
            const assigned = [...(asg?.presets ?? []), ...(asg?.copies ?? [])];
            return (
              <div key={c.id} className="bg-canvas px-3 py-2.5">
                <div className="flex items-baseline gap-2 min-w-0">
                  <span
                    className={`status-dot ${
                      c.paused
                        ? "amber"
                        : c.runtime_state?.includes("ready")
                          ? "green"
                          : "grey"
                    }`}
                    style={{ alignSelf: "center" }}
                  />
                  <span className="text-[12px] font-medium text-ink-1 truncate">
                    {c.label ?? shortAddr(c.address, 4, 4)}
                  </span>
                  {(c.id.startsWith("gw-") || c.runtime_state === "remote") && (
                    <span
                      className="badge"
                      title="Remote: registered on your DegenBox account but running on another device (or as a gateway-side binding). This app can't sign for it."
                    >
                      remote
                    </span>
                  )}
                  <span className="ml-auto font-mono text-[10px] text-ink-4">
                    {shortAddr(c.address, 4, 4)}
                  </span>
                </div>
                <div
                  className="mt-0.5 text-[10px] font-mono text-ink-4 uppercase tracking-wider truncate"
                  title={assigned.join(", ")}
                >
                  {assigned.length > 0 ? assigned.join(" · ") : "no preset / target"}
                </div>
                <div className="mt-1.5 flex items-baseline gap-3 font-mono tabular-nums text-[11px]">
                  <span title="Wallet SOL balance">
                    <span className="text-ink-4">bal </span>
                    <span className="text-ink-1">{bal != null ? fmtSolAmt(bal) : "—"}</span>
                    <span className="text-ink-4"> sol</span>
                  </span>
                  <span title="Unrealized PnL (attributed open positions)">
                    <span className="text-ink-4">upnl </span>
                    <span className={toneCls(upnlVal)}>{signed(upnlVal, unit)}</span>
                  </span>
                </div>
                <div
                  className="mt-1 flex items-baseline gap-3 font-mono tabular-nums text-[11px]"
                  title={`Realized PnL (${unit === "sol" ? "SOL" : "USD"}): 1d / 7d / 30d`}
                >
                  {(["d1", "d7", "d30"] as const).map((k, i) => {
                    const v = w(k);
                    return (
                      <span key={k}>
                        <span className="text-ink-4">{["d ", "w ", "m "][i]}</span>
                        <span className={toneCls(v)}>{signed(v, unit)}</span>
                      </span>
                    );
                  })}
                </div>
              </div>
            );
          })}
        </div>
      )}

      {/* Global sum rail — THE one glow + corner-bracket focus on this
          screen (decoration budget: signature element only). */}
      <div className="corners glow-accent border border-accent/25 bg-card/75 px-3 py-2 flex items-baseline gap-5 font-mono tabular-nums text-[12px] mt-px">
        <span className="hud-label brackets">Total</span>
        <span title="Sum of client wallet balances">
          <span className="text-ink-4">bal </span>
          <span className="text-ink-1">{balTotal != null ? fmtSolAmt(balTotal) : "—"}</span>
          <span className="text-ink-4"> sol</span>
        </span>
        <span title="Unrealized PnL across all open positions">
          <span className="text-ink-4">upnl </span>
          <span className={`glow-text ${toneCls(upnlSum)}`}>{signed(upnlSum, unit)}</span>
        </span>
        <span className="ml-auto flex items-baseline gap-4" title="Realized PnL: 1d / 7d / 30d">
          {(["d1", "d7", "d30"] as const).map((k, i) => {
            const v = realizedWindow(k);
            return (
              <span key={k}>
                <span className="text-ink-4">{["d ", "w ", "m "][i]}</span>
                <span className={toneCls(v)}>{signed(v, unit)}</span>
              </span>
            );
          })}
        </span>
      </div>
    </div>
  );
}
