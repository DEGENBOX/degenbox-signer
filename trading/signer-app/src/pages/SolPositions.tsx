// Solana → Positions (W3.1 bot-redesign) — the flagship surface.
//
// Anatomy:
//   01 / CLIENTS  — corner-bracketed card per Sol client (balance,
//                   uPnL, assigned presets/copy targets, d/w/m realized
//                   PnL from GET /api/trading/pnl/windows) + a global
//                   sum rail (the screen's single glow focus).
//   02 / POSITIONS — terminal-dense table: coin, entry/now MCAP,
//                   value + uPnL with a global $⇄SOL toggle
//                   (persisted), source attribution (client + preset /
//                   copy target via the intents ledger), TP/SL ladder
//                   controls incl. one-click break-even stop, quick
//                   25/50/100% sells, row-expand custom sell + live
//                   candle chart (ported TokenChart, 5 s tail poll).
//
// Sells execute through THIS device's signer engine (on-chain
// balance-clamped, native routing + Jupiter fallback) and route to the
// wallet that actually HOLDS the position: the intents-ledger
// attribution travels as `ownerPubkey` and the backend verifies it
// against real on-chain holdings (audit N2) — ambiguous cases refuse
// loudly instead of defaulting to the primary.

import { useCallback, useEffect, useMemo, useState } from "react";
import { ChevronRight, Coins, Crosshair, RefreshCw, Shield } from "lucide-react";
import {
  ipc,
  isLiveTargetStatus,
  type ClientInfo,
  type LegSpec,
  type PositionTargetRow,
} from "../ipc";
import {
  EmptyState,
  Modal,
  Segmented,
  SkeletonRows,
  fmtUsd,
  shortAddr,
  timeAgo,
} from "../components/ui";
import { ArmLadderDialog } from "../components/ArmLadderDialog";
import { PnlText, TokenAvatar } from "@degenbox/ui";
import { solPositionsEx, type SolPositionEx } from "../features/positions/ipc";
import {
  compactNum,
  fetchSourceMap,
  fmtSolAmt,
  lamportsToSol,
  num,
  type PositionSource,
} from "../features/positions/data";
import { ClientsStrip, type Unit, type UpnlSums } from "../features/positions/ClientsStrip";
import { PositionChart } from "../features/positions/chart/PositionChart";
import { getSkipCloseConfirm } from "../lib/prefs";

const POLL_MS = 10_000;
const CLIENTS_POLL_MS = 15_000;
const SOURCES_POLL_MS = 30_000;
const UNIT_KEY = "degenbox.signer.positions.unit";

function loadUnit(): Unit {
  try {
    const u = localStorage.getItem(UNIT_KEY);
    if (u === "sol" || u === "usd") return u;
  } catch {
    // storage unavailable
  }
  return "usd";
}

interface PendingSell {
  position: SolPositionEx;
  fractionBps: number;
  /** Human description for the confirm ("50%", "0.25 SOL ≈ 31%"). */
  label: string;
  /** Wallet the position is genuinely attributed to (intents ledger) —
   *  null when unattributed; the backend then resolves the holder
   *  on-chain and refuses multi-wallet ambiguity. NEVER the
   *  fold-to-oldest display default (that guess must not pick which
   *  wallet a sell fires from). */
  owner: { address: string; label: string } | null;
}

const isSolClient = (c: ClientInfo) =>
  c.chain === "sol" || c.gateway?.chain === "solana";

export function SolPositions({ embedded }: { embedded?: boolean } = {}) {
  const [positions, setPositions] = useState<SolPositionEx[] | null>(null);
  const [targets, setTargets] = useState<PositionTargetRow[] | null>(null);
  const [clients, setClients] = useState<ClientInfo[] | null>(null);
  const [sourceMap, setSourceMap] = useState<Map<string, PositionSource>>(new Map());
  const [presetNames, setPresetNames] = useState<Map<string, string>>(new Map());
  const [copyLabels, setCopyLabels] = useState<Map<string, string>>(new Map());
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [unit, setUnitState] = useState<Unit>(() => loadUnit());

  // Row expand (one at a time — the chart poll is per-mint).
  const [expanded, setExpanded] = useState<string | null>(null);

  // Sell flow.
  const [pendingSell, setPendingSell] = useState<PendingSell | null>(null);
  const [sellBusy, setSellBusy] = useState(false);
  const [sellErr, setSellErr] = useState<string | null>(null);
  const [lastSell, setLastSell] = useState<{ symbol: string; signature: string } | null>(
    null,
  );
  // Custom sell (expanded-row inline).
  const [customText, setCustomText] = useState("");
  const [customUnit, setCustomUnit] = useState<"pct" | "sol">("pct");

  // TP/SL flow.
  const [armFor, setArmFor] = useState<SolPositionEx | null>(null);
  const [beBusyMint, setBeBusyMint] = useState<string | null>(null);
  const [beErr, setBeErr] = useState<string | null>(null);

  const setUnit = useCallback((u: Unit) => {
    setUnitState(u);
    try {
      localStorage.setItem(UNIT_KEY, u);
    } catch {
      // session-only
    }
  }, []);

  // ── feeds ──────────────────────────────────────────────────────────

  const load = useCallback(async () => {
    setBusy(true);
    const [pos, tgts] = await Promise.allSettled([solPositionsEx(), ipc.solTargetsList()]);
    if (pos.status === "fulfilled") {
      setPositions(pos.value);
      setErr(null);
    } else {
      setErr(String(pos.reason));
    }
    if (tgts.status === "fulfilled") setTargets(tgts.value);
    setBusy(false);
  }, []);

  useEffect(() => {
    load();
    const id = setInterval(load, POLL_MS);
    return () => clearInterval(id);
  }, [load]);

  useEffect(() => {
    let alive = true;
    const loadClients = () =>
      ipc.clientsList().then(
        (list) => {
          if (alive) setClients(list.filter(isSolClient));
        },
        () => {
          // keep last snapshot
        },
      );
    loadClients();
    const id = setInterval(loadClients, CLIENTS_POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  // Source attribution (intents ledger) + name lookups.
  useEffect(() => {
    let alive = true;
    const loadSources = async () => {
      const [src, presets, copies] = await Promise.allSettled([
        fetchSourceMap(),
        ipc.alphaPresets(),
        ipc.copytradeConfigs(),
      ]);
      if (!alive) return;
      if (src.status === "fulfilled") setSourceMap(src.value);
      if (presets.status === "fulfilled") {
        setPresetNames(new Map(presets.value.map((p) => [p.id, p.name])));
      }
      if (copies.status === "fulfilled") {
        setCopyLabels(
          new Map(
            copies.value.filter((c) => c.venue === "solana").map((c) => [c.id, c.label]),
          ),
        );
      }
    };
    loadSources();
    const id = setInterval(loadSources, SOURCES_POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  // ── attribution + strip rollups ────────────────────────────────────

  const liveTargetFor = useCallback(
    (mint: string): PositionTargetRow | null =>
      targets?.find((t) => t.mint === mint && isLiveTargetStatus(t.status)) ?? null,
    [targets],
  );

  /** Oldest live Sol gateway client — the fold target for legacy /
   *  unattributed positions (mirrors the PnL-windows endpoint rule). */
  const oldestGwId = useMemo(() => {
    let best: { id: string; at: string } | null = null;
    for (const c of clients ?? []) {
      const gw = c.gateway;
      if (!gw) continue;
      const at = gw.created_at ?? "9999";
      if (!best || at < best.at) best = { id: gw.id, at };
    }
    return best?.id ?? null;
  }, [clients]);

  const gwIds = useMemo(
    () => new Set((clients ?? []).map((c) => c.gateway?.id).filter(Boolean) as string[]),
    [clients],
  );

  /** mint → gateway client id (best-effort, fold to oldest).
   *  DISPLAY-ONLY: rollups + the Source cell. Sell routing must use
   *  `attributedWallet` (no fold) — the fold is a guess. */
  const clientForMint = useCallback(
    (mint: string): string | null => {
      const src = sourceMap.get(mint);
      if (src?.clientId && gwIds.has(src.clientId)) return src.clientId;
      return oldestGwId;
    },
    [sourceMap, gwIds, oldestGwId],
  );

  /** Wallet that genuinely owns this position per the intents ledger —
   *  null when unattributed (no fold-to-oldest!). Travels with the sell
   *  so the backend executes from the holding wallet (audit N2). */
  const attributedWallet = useCallback(
    (mint: string): { address: string; label: string } | null => {
      const src = sourceMap.get(mint);
      if (!src?.clientId || !gwIds.has(src.clientId)) return null;
      const c = (clients ?? []).find((x) => x.gateway?.id === src.clientId);
      if (!c) return null;
      return { address: c.address, label: c.label ?? shortAddr(c.address, 4, 4) };
    },
    [sourceMap, gwIds, clients],
  );

  const { upnlByGw, upnlTotal } = useMemo(() => {
    const byGw = new Map<string, UpnlSums>();
    const total: UpnlSums = { usd: null, sol: null };
    for (const p of positions ?? []) {
      const pnlUsd = num(p.pnl_usd);
      const pnlSol = num(p.pnl_sol);
      if (pnlUsd != null) total.usd = (total.usd ?? 0) + pnlUsd;
      if (pnlSol != null) total.sol = (total.sol ?? 0) + pnlSol;
      const gw = clientForMint(p.mint);
      if (!gw) continue;
      const acc = byGw.get(gw) ?? { usd: null, sol: null };
      if (pnlUsd != null) acc.usd = (acc.usd ?? 0) + pnlUsd;
      if (pnlSol != null) acc.sol = (acc.sol ?? 0) + pnlSol;
      byGw.set(gw, acc);
    }
    return { upnlByGw: byGw, upnlTotal: total };
  }, [positions, clientForMint]);

  /** Source cell labels: [client, detail]. */
  const sourceLabels = useCallback(
    (mint: string): [string, string] => {
      const src = sourceMap.get(mint);
      const gwId = clientForMint(mint);
      const client = (clients ?? []).find((c) => c.gateway?.id === gwId);
      const clientLabel = client
        ? (client.label ?? shortAddr(client.address, 4, 4))
        : "—";
      let detail = "manual";
      if (src?.presetId) detail = presetNames.get(src.presetId) ?? "preset";
      else if (src?.copyConfigId) {
        detail = `copy ${copyLabels.get(src.copyConfigId) ?? ""}`.trim();
      } else if (!src) detail = "—";
      return [clientLabel, detail];
    },
    [sourceMap, clientForMint, clients, presetNames, copyLabels],
  );

  // ── actions ────────────────────────────────────────────────────────

  // Core sell — used by both the confirm-modal path and the direct
  // ("Don't ask again") path.
  const doSell = async (sell: PendingSell) => {
    setSellBusy(true);
    setSellErr(null);
    try {
      const res = await ipc.solPositionSell(
        sell.position.mint,
        sell.fractionBps,
        sell.owner?.address ?? null,
      );
      setLastSell({ symbol: sell.position.symbol, signature: res.signature });
      setPendingSell(null);
      setCustomText("");
      await load();
    } catch (e) {
      setSellErr(String(e));
    } finally {
      setSellBusy(false);
    }
  };

  const runSell = () => {
    if (pendingSell) doSell(pendingSell);
  };

  // Close/sell entrypoint honouring the shared `dbx.skipCloseConfirm`
  // pref: when set, fire the sell directly at the requested size (no
  // confirm modal); otherwise open the confirm as before. The backend
  // still clamps to on-chain balance and refuses ambiguous holders.
  const requestSell = (sell: PendingSell) => {
    setSellErr(null);
    if (getSkipCloseConfirm()) {
      doSell(sell);
    } else {
      setPendingSell(sell);
    }
  };

  const queueQuickSell = (p: SolPositionEx, bps: number) => {
    requestSell({
      position: p,
      fractionBps: bps,
      label: `${bps / 100}%`,
      owner: attributedWallet(p.mint),
    });
  };

  /** Custom sell → fraction bps. % maps directly; SOL maps via the
   *  position's live SOL value. */
  const customBps = (p: SolPositionEx): { bps: number; label: string } | null => {
    const v = num(customText);
    if (v == null || v <= 0) return null;
    if (customUnit === "pct") {
      const bps = Math.min(10_000, Math.max(1, Math.round(v * 100)));
      return v > 100 ? null : { bps, label: `${v}%` };
    }
    const valueSol = num(p.value_sol);
    if (valueSol == null || valueSol <= 0) return null;
    const bps = Math.min(10_000, Math.max(1, Math.round((v / valueSol) * 10_000)));
    return { bps, label: `${v} SOL ≈ ${(bps / 100).toFixed(1)}%` };
  };

  /** One-click break-even stop: re-arm the ladder anchored at the LIVE
   *  price with an SL leg whose level sits exactly at the avg entry —
   *  existing live TP legs are carried over at the same absolute
   *  levels. Only meaningful while in profit (a BE level above the
   *  current price would fire instantly). */
  const breakEvenDisabled = (p: SolPositionEx): string | null => {
    const price = num(p.current_price_usd);
    const entry = num(p.avg_entry_price_usd);
    if (price == null || entry == null) return "needs live price + cost basis";
    if (price <= entry * 1.001) return "position not in profit: stop at entry would fire immediately";
    return null;
  };

  const armBreakEven = async (p: SolPositionEx) => {
    if (breakEvenDisabled(p)) return;
    const price = num(p.current_price_usd)!;
    const entry = num(p.avg_entry_price_usd)!;
    setBeBusyMint(p.mint);
    setBeErr(null);
    try {
      const legs: LegSpec[] = [];
      const live = liveTargetFor(p.mint);
      if (live) {
        const oldEntry = Number(live.entry_price_usd);
        const seen = new Set<string>();
        for (const l of live.legs ?? []) {
          if (l.kind !== "tp") continue;
          if (!(l.status === "active" || l.status === "firing")) continue;
          // Same absolute level, re-expressed vs. the new anchor.
          const level = oldEntry * (1 + Number(l.trigger_pct) / 100);
          const pct = ((level / price - 1) * 100).toFixed(2);
          if (Number(pct) <= 0.01 || seen.has(pct)) continue; // already passed / dup
          seen.add(pct);
          legs.push({
            kind: "tp",
            trigger_pct: pct,
            sell_fraction_bps: l.sell_fraction_bps,
          });
        }
      }
      const slPct = Math.max((1 - entry / price) * 100, 0.05);
      legs.push({
        kind: "sl",
        trigger_pct: slPct.toFixed(2),
        sell_fraction_bps: 10_000,
      });
      await ipc.solTargetArm(p.mint, String(price), legs);
      await load();
    } catch (e) {
      setBeErr(String(e));
    } finally {
      setBeBusyMint(null);
    }
  };

  // ── render ─────────────────────────────────────────────────────────

  const cols = 10;
  const valSuffix = unit === "sol" ? " sol" : "";

  const fmtVal = (usd: string | null, sol: string | null): string => {
    if (unit === "usd") return fmtUsd(usd);
    const v = num(sol);
    return v == null ? "—" : fmtSolAmt(v);
  };
  const fmtPnlCell = (usd: string | null, sol: string | null): string => {
    const v = unit === "usd" ? num(usd) : num(sol);
    if (v == null) return "—";
    const sign = v > 0 ? "+" : "";
    return unit === "usd" ? `${sign}${fmtUsd(usd)}` : `${sign}${fmtSolAmt(v)}`;
  };

  return (
    <>
      {!embedded && (
        <>
          <h1>Positions</h1>
          <p className="page-sub">
            Open Solana spot. Sells execute through this device's signer engine; TP/SL
            ladders run on the gateway.
          </p>
        </>
      )}

      <ClientsStrip
        clients={clients}
        upnlByGw={upnlByGw}
        upnlTotal={upnlTotal}
        unit={unit}
      />

      {lastSell && (
        <div className="banner" role="status">
          <span style={{ flex: 1 }}>
            Sell submitted for <strong>{lastSell.symbol}</strong>:{" "}
            <span className="mono">{shortAddr(lastSell.signature, 8, 8)}</span>
          </span>
          <button className="btn" onClick={() => setLastSell(null)}>
            Dismiss
          </button>
        </div>
      )}
      {beErr && <div className="error-box">break-even stop: {beErr}</div>}
      {sellErr && pendingSell === null && <div className="error-box">sell: {sellErr}</div>}

      <div className="shell-section-head">
        <span className="section-num">02</span>
        <span className="shell-section-title">Open positions</span>
        <span className="hud-label brackets">{positions?.length ?? "–"}</span>
        <span className="head-meta">
          <Segmented<Unit>
            value={unit}
            onChange={setUnit}
            options={[
              { value: "usd", label: "$" },
              { value: "sol", label: "SOL" },
            ]}
          />
          <button
            className="btn sm"
            disabled={busy}
            onClick={load}
            title="Refresh positions"
          >
            <RefreshCw size={12} />
          </button>
        </span>
      </div>

      <div className="card" style={{ paddingTop: 12, paddingBottom: 8 }}>
        {err ? (
          <div className="error-box">{err}</div>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Coin</th>
                <th className="num">Entry MC</th>
                <th className="num">MC now</th>
                <th className="num">Value{valSuffix && <span> (sol)</span>}</th>
                <th className="num">PnL{valSuffix && <span> (sol)</span>}</th>
                <th className="num">%</th>
                <th>Source</th>
                <th>TP/SL</th>
                <th style={{ textAlign: "right" }}>Sell</th>
                <th style={{ width: 26 }} />
              </tr>
            </thead>
            <tbody>
              {positions === null ? (
                <SkeletonRows rows={3} cols={cols} />
              ) : positions.length === 0 ? (
                <tr>
                  <td colSpan={cols}>
                    <EmptyState
                      icon={<Coins size={18} />}
                      title="No open Solana positions"
                      hint="preset + copy-trade buys land here"
                    />
                  </td>
                </tr>
              ) : (
                positions.map((p) => {
                  const live = liveTargetFor(p.mint);
                  const liveLegs = (live?.legs ?? []).filter(
                    (l) => l.status === "active" || l.status === "firing",
                  ).length;
                  const isOpen = expanded === p.mint;
                  const cost = num(p.cost_usd);
                  const pnl = num(p.pnl_usd);
                  const pct = cost != null && cost > 0 && pnl != null ? (pnl / cost) * 100 : null;
                  const [srcClient, srcDetail] = sourceLabels(p.mint);
                  const beDisabled = breakEvenDisabled(p);
                  const supply = (() => {
                    const m = num(p.mcap_usd);
                    const pr = num(p.current_price_usd);
                    return m != null && pr != null && pr > 0 ? m / pr : null;
                  })();
                  return (
                    <FragmentRow key={p.mint}>
                      <tr
                        onClick={() => setExpanded(isOpen ? null : p.mint)}
                        style={{ cursor: "pointer" }}
                      >
                        <td>
                          <span className="flex items-center gap-2 min-w-0">
                            <TokenAvatar imageUrl={p.image_url} symbol={p.symbol} size={22} />
                            <span className="min-w-0">
                              <strong>{p.symbol}</strong>{" "}
                              <span className="mono" style={{ color: "var(--fg-faint)", fontSize: 10 }}>
                                {shortAddr(p.mint, 4, 4)}
                              </span>
                            </span>
                          </span>
                        </td>
                        <td className="num">{compactNum(num(p.entry_mcap_usd))}</td>
                        <td className="num">{compactNum(num(p.mcap_usd))}</td>
                        <td className="num">{fmtVal(p.value_usd, p.value_sol)}</td>
                        <td className="num">
                          <span className={pnl == null ? "" : pnl > 0 ? "pos" : pnl < 0 ? "neg" : ""}>
                            {fmtPnlCell(p.pnl_usd, p.pnl_sol)}
                          </span>
                        </td>
                        <td className="num">
                          <PnlText pct={pct} digits={1} className="text-[11px]" />
                        </td>
                        <td>
                          <span className="block text-[11px] text-ink-2 truncate" style={{ maxWidth: 110 }}>
                            {srcClient}
                          </span>
                          <span className="block font-mono text-[10px] text-ink-4 truncate" style={{ maxWidth: 110 }}>
                            {srcDetail}
                          </span>
                        </td>
                        <td onClick={(e) => e.stopPropagation()} style={{ whiteSpace: "nowrap" }}>
                          <button
                            className="btn xs"
                            title={
                              live
                                ? `armed · entry $${Number(live.entry_price_usd)} · ${liveLegs} live leg(s) · edit / disarm`
                                : "Arm a TP/SL ladder on this position"
                            }
                            onClick={() => setArmFor(p)}
                          >
                            <Crosshair size={11} />{" "}
                            {live ? (
                              <span className="pos">{liveLegs} leg{liveLegs === 1 ? "" : "s"}</span>
                            ) : (
                              <span style={{ color: "var(--fg-faint)" }}>arm</span>
                            )}
                          </button>
                          <button
                            className="btn xs"
                            style={{ marginLeft: 4 }}
                            disabled={beDisabled != null || beBusyMint === p.mint}
                            title={
                              beDisabled ??
                              "Move the stop to break-even: SL at your avg entry, TP legs kept"
                            }
                            onClick={() => armBreakEven(p)}
                          >
                            <Shield size={11} /> {beBusyMint === p.mint ? "…" : "BE"}
                          </button>
                        </td>
                        <td
                          onClick={(e) => e.stopPropagation()}
                          style={{ textAlign: "right", whiteSpace: "nowrap" }}
                        >
                          {[2500, 5000, 10000].map((bps) => (
                            <button
                              key={bps}
                              className={`btn xs ${bps === 10000 ? "danger" : ""}`}
                              style={{ marginLeft: 4 }}
                              title={`Sell ${bps / 100}% of the on-chain balance via this device's signer`}
                              onClick={() => queueQuickSell(p, bps)}
                            >
                              {bps / 100}
                            </button>
                          ))}
                        </td>
                        <td style={{ textAlign: "right", color: "var(--fg-faint)" }}>
                          <ChevronRight size={13} className={`chev ${isOpen ? "open" : ""}`} />
                        </td>
                      </tr>
                      {isOpen && (
                        <tr>
                          <td colSpan={cols} style={{ padding: "10px 8px 14px 0" }}>
                            <div className="expand-in">
                            <div className="flex flex-wrap items-end gap-4 mb-2.5">
                              <div className="flex items-end gap-1.5">
                                <input
                                  className="input mono"
                                  style={{ width: 130 }}
                                  inputMode="decimal"
                                  value={customText}
                                  onChange={(e) => setCustomText(e.target.value)}
                                  placeholder={customUnit === "pct" ? "33" : "0.25"}
                                  aria-label="custom sell amount"
                                />
                                <Segmented<"pct" | "sol">
                                  value={customUnit}
                                  onChange={setCustomUnit}
                                  options={[
                                    { value: "pct", label: "%" },
                                    { value: "sol", label: "SOL" },
                                  ]}
                                />
                                <button
                                  className="btn sm danger"
                                  disabled={customBps(p) == null}
                                  title={
                                    customBps(p) == null
                                      ? "enter an amount (needs a live SOL value for SOL-sized sells)"
                                      : `Sell ${customBps(p)!.label} of ${p.symbol}`
                                  }
                                  onClick={() => {
                                    const c = customBps(p);
                                    if (!c) return;
                                    requestSell({
                                      position: p,
                                      fractionBps: c.bps,
                                      label: c.label,
                                      owner: attributedWallet(p.mint),
                                    });
                                  }}
                                >
                                  Sell
                                </button>
                              </div>
                              <div className="ml-auto flex items-baseline gap-4 font-mono tabular-nums text-[11px]">
                                <span title="Average entry price per token">
                                  <span className="text-ink-4">entry </span>
                                  <span className="text-ink-2">
                                    {p.avg_entry_price_usd
                                      ? `$${Number(p.avg_entry_price_usd).toPrecision(4)}`
                                      : "—"}
                                  </span>
                                </span>
                                <span title="Realized PnL banked on this mint">
                                  <span className="text-ink-4">realized </span>
                                  <span
                                    className={
                                      p.realized_pnl_lamports > 0
                                        ? "text-up"
                                        : p.realized_pnl_lamports < 0
                                          ? "text-down"
                                          : "text-ink-3"
                                    }
                                  >
                                    {p.realized_pnl_lamports === 0
                                      ? "0"
                                      : `${p.realized_pnl_lamports > 0 ? "+" : ""}${fmtSolAmt(
                                          lamportsToSol(p.realized_pnl_lamports),
                                        )} sol`}
                                  </span>
                                </span>
                                <span title="Lifetime fills on this position">
                                  <span className="text-ink-4">fills </span>
                                  <span className="text-ink-2">{p.fill_count}</span>
                                </span>
                                <span title="Opened">
                                  <span className="text-ink-4">opened </span>
                                  <span className="text-ink-2">{timeAgo(p.opened_at)}</span>
                                </span>
                              </div>
                            </div>
                            <PositionChart
                              address={p.mint}
                              symbol={p.symbol}
                              supply={supply}
                              entryPriceUsd={num(p.avg_entry_price_usd)}
                              currentPriceUsd={num(p.current_price_usd)}
                              height={300}
                            />
                            </div>
                          </td>
                        </tr>
                      )}
                    </FragmentRow>
                  );
                })
              )}
            </tbody>
          </table>
        )}
      </div>

      {/* Sell confirm */}
      <Modal
        open={pendingSell !== null}
        onClose={() => (sellBusy ? undefined : setPendingSell(null))}
        title={
          pendingSell ? `Sell ${pendingSell.label} · ${pendingSell.position.symbol}` : "Sell"
        }
        width={420}
        locked={sellBusy}
      >
        {pendingSell && (
          <>
            <p style={{ marginTop: 0 }}>
              Sell <strong>{pendingSell.label}</strong>
              {pendingSell.fractionBps !== 10000 && (
                <> ({(pendingSell.fractionBps / 100).toFixed(1)}% of the holding)</>
              )}{" "}
              of the on-chain <strong>{pendingSell.position.symbol}</strong> balance{" "}
              {pendingSell.owner ? (
                <>
                  held by client <strong>{pendingSell.owner.label}</strong>{" "}
                  <span className="mono" style={{ fontSize: 11 }}>
                    {shortAddr(pendingSell.owner.address, 4, 4)}
                  </span>
                </>
              ) : (
                <>held by the wallet this position resolves to on-chain</>
              )}{" "}
              for SOL, signed and submitted by this device. The amount is clamped to what
              that wallet actually holds right now
              {pendingSell.position.value_usd
                ? ` (position ≈ ${fmtUsd(pendingSell.position.value_usd)})`
                : ""}
              ; if the holding wallet can't be determined unambiguously, the sell is
              refused rather than guessed.
            </p>
            {sellErr && <div className="error-box">{sellErr}</div>}
            <div className="modal-foot">
              <button className="btn" disabled={sellBusy} onClick={() => setPendingSell(null)}>
                Cancel
              </button>
              <button className="btn danger solid" disabled={sellBusy} onClick={runSell}>
                {sellBusy ? "Selling…" : `Sell ${pendingSell.label}`}
              </button>
            </div>
          </>
        )}
      </Modal>

      {/* TP/SL arm/edit — existing machinery, reused. */}
      <ArmLadderDialog
        open={armFor !== null}
        onClose={() => setArmFor(null)}
        onChanged={load}
        mint={armFor?.mint ?? ""}
        symbol={armFor?.symbol ?? ""}
        suggestedEntryUsd={armFor?.current_price_usd ?? null}
        existing={armFor ? liveTargetFor(armFor.mint) : null}
      />
    </>
  );
}

/** Keyed fragment helper so the (row, expanded-row) pair shares one key. */
function FragmentRow({ children }: { children: React.ReactNode }) {
  return <>{children}</>;
}
