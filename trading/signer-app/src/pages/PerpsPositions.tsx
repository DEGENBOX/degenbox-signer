// Perpetuals → Positions (W4.1 bot-redesign) — exact layout mirror of
// the Solana Positions page, themed by `.mode-perps` (indigo/violet
// accents flip on <html>; zero theme tokens live here).
//
// Anatomy:
//   01 / ACCOUNT   — corner-bracketed card for the HL master account
//                    (this device is the SOLE Perpetuals executor):
//                    equity, withdrawable, uPnL, d/w/m realized PnL
//                    from GET /api/hyperliquid/pnl/windows + a global
//                    sum rail (the screen's single glow focus).
//   02 / POSITIONS — terminal-dense table: coin + size, side × lev,
//                    entry, mark, value, uPnL $ / %, source attribution
//                    (gateway intent/caller ledger), TP/SL triggers,
//                    quick 25/50/100 closes, row-expand custom close +
//                    live HL candle chart (entry / NOW / LIQ lines,
//                    5 s tail poll via the gateway candle proxy).
//
// Data: the daemon's `hl_status` balance snapshot stays the source of
// truth for what's open (and what the close/TPSL controls operate on);
// the gateway's clearinghouse view (/wallets/{addr}/positions, 30 s
// cache) layers on mark/leverage/liquidation/source. Closes + triggers
// execute reduce-only through THIS device's signer at live size.

import { useCallback, useEffect, useMemo, useState } from "react";
import {
  ChevronRight,
  Crosshair,
  Link2,
  RefreshCw,
  Scissors,
  TrendingUp,
} from "lucide-react";
import { ipc, type HlPosition, type HlStatus } from "../ipc";
import { getSkipCloseConfirm } from "../lib/prefs";
import { EmptyHero, EmptyState, SkeletonRows, fmtUsd, timeAgo } from "../components/ui";
import { PnlText } from "@degenbox/ui";
import {
  compactNum,
  deriveMark,
  fetchGwPositions,
  formatPerpPrice,
  num,
  sourceLabel,
  type GwPerpPosition,
} from "../features/perps-positions/data";
import { AccountStrip } from "../features/perps-positions/AccountStrip";
import {
  ClosePositionDialog,
  TpslDialog,
} from "../features/perps-positions/dialogs";
import { PerpChart } from "../features/perps-positions/chart/PerpChart";

const GW_POSITIONS_POLL_MS = 30_000; // matches the gateway cache TTL

interface Props {
  hl: HlStatus | null;
  onReload: () => void;
  /** Jump to the Bots tab (pairing lives there). */
  onGoOverview: () => void;
  /** Rendered inside the LIVE tab wrapper — suppress the page title. */
  embedded?: boolean;
}

interface PendingClose {
  position: HlPosition;
  percent: number;
}

export function PerpsPositions({ hl, onReload, onGoOverview, embedded }: Props) {
  const loading = hl === null;
  const balErr = hl?.balance.error ?? null;
  const positions = useMemo(() => hl?.balance.positions ?? [], [hl]);
  const account = hl?.account_address ?? null;

  // Gateway enrichment (mark / leverage / liq / source) keyed by coin.
  const [gwByCoin, setGwByCoin] = useState<Map<string, GwPerpPosition>>(new Map());

  // Row expand (one at a time — the chart poll is per-coin).
  const [expanded, setExpanded] = useState<string | null>(null);

  // Close / TP-SL flows.
  const [pendingClose, setPendingClose] = useState<PendingClose | null>(null);
  const [tpslFor, setTpslFor] = useState<HlPosition | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // Custom close (expanded-row inline).
  const [customText, setCustomText] = useState("");

  // ── feeds ──────────────────────────────────────────────────────────
  // The daemon snapshot itself is App-polled (2 s) via the `hl` prop;
  // this page only owns the gateway enrichment poll.

  useEffect(() => {
    if (!account) return;
    let alive = true;
    const load = () =>
      fetchGwPositions(account).then(
        (rows) => {
          if (!alive) return;
          setGwByCoin(new Map(rows.map((r) => [r.coin.toUpperCase(), r])));
        },
        () => {
          // 403 (sub) / 502 / transient — degrade to the snapshot
          // fields; mark falls back to the uPnL-derived value.
        },
      );
    load();
    const id = setInterval(load, GW_POSITIONS_POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [account]);

  const totalUpnl = useMemo(() => {
    let sum = 0;
    let any = false;
    for (const p of positions) {
      const n = num(p.unrealized_pnl);
      if (n != null) {
        sum += n;
        any = true;
      }
    }
    return any ? sum : null;
  }, [positions]);

  // ── per-row derivation ─────────────────────────────────────────────

  const rowFacts = useCallback(
    (p: HlPosition) => {
      const gw = gwByCoin.get(p.coin.toUpperCase());
      const szi = num(p.szi);
      const entry = num(p.entry_px);
      const upnl = num(p.unrealized_pnl);
      const mark = num(gw?.mark_px ?? null) ?? deriveMark(p.entry_px, p.unrealized_pnl, p.szi);
      const sizeAbs = szi != null ? Math.abs(szi) : null;
      const value = sizeAbs != null && mark != null ? sizeAbs * mark : null;
      const margin = num(gw?.margin_used ?? null);
      const notionalEntry = sizeAbs != null && entry != null ? sizeAbs * entry : null;
      // ROE when margin is known (HL convention); unlevered return as
      // the degraded fallback.
      const pct =
        upnl != null && margin != null && margin > 0
          ? (upnl / margin) * 100
          : upnl != null && notionalEntry != null && notionalEntry > 0
            ? (upnl / notionalEntry) * 100
            : null;
      return {
        gw,
        szi,
        sizeAbs,
        entry,
        mark,
        value,
        upnl,
        pct,
        roe: margin != null && margin > 0,
        leverage: gw?.leverage ?? null,
        liq: num(gw?.liquidation_px ?? null),
        margin,
        funding: num(gw?.funding_since_open ?? null),
        source: gw?.source ?? null,
      };
    },
    [gwByCoin],
  );

  const customPct = (): number | null => {
    const v = num(customText);
    if (v == null || v <= 0 || v > 100) return null;
    return v;
  };

  // Close entrypoint. When the operator has opted out of the confirm
  // dialog ("Don't ask again"), fire the reduce-only close directly at
  // the requested percent (no type-to-confirm friction); otherwise open
  // the type-to-confirm dialog as before.
  const requestClose = useCallback(
    async (p: HlPosition, percent: number) => {
      if (!getSkipCloseConfirm()) {
        setPendingClose({ position: p, percent });
        return;
      }
      try {
        const res = await ipc.hlClosePosition(p.coin, percent);
        setNotice(
          res.status === "paper"
            ? `Paper mode: ${percent}% close of ${p.coin} recorded (no live order).`
            : `Close queued for your signer: ${percent}% of ${p.coin} (${res.cloid}).`,
        );
        setCustomText("");
        onReload();
      } catch (e) {
        setNotice(`Close failed for ${p.coin}: ${String(e)}`);
      }
    },
    [onReload],
  );

  // ── render ─────────────────────────────────────────────────────────

  const cols = 11;

  if (!loading && !hl.paired) {
    return (
      <>
        {!embedded && (
          <>
            <h1>Positions</h1>
            <p className="page-sub">Open Perpetuals positions on your master account.</p>
          </>
        )}
        <AccountStrip hl={hl} upnlUsd={null} />
        <EmptyHero
          icon={<Link2 size={22} />}
          title="Not paired with DegenBox yet"
          desc="Pair this device on the Bots tab to see live positions and balances."
          action={
            <button className="btn primary lg" onClick={onGoOverview}>
              <Link2 size={15} /> Go to Perpetuals setup
            </button>
          }
        />
      </>
    );
  }

  return (
    <>
      {!embedded && (
        <>
          <h1>Positions</h1>
          <p className="page-sub">
            Open Perpetuals positions on your master account. Closes and TP/SL triggers
            execute reduce-only through this device's signer at live size.
          </p>
        </>
      )}

      <AccountStrip hl={hl} upnlUsd={totalUpnl} />

      {notice && (
        <div className="banner" role="status">
          <span style={{ flex: 1 }}>{notice}</span>
          <button className="btn" onClick={() => setNotice(null)}>
            Dismiss
          </button>
        </div>
      )}

      <div className="shell-section-head">
        <span className="section-num">02</span>
        <span className="shell-section-title">Open positions</span>
        <span className="hud-label brackets">{hl ? positions.length : "–"}</span>
        <span className="head-meta">
          {hl?.balance.fetched_at && (
            <span className="hud-label" title="Balance snapshot age">
              {timeAgo(hl.balance.fetched_at)}
            </span>
          )}
          <button className="btn sm" onClick={onReload} title="Refresh positions">
            <RefreshCw size={12} />
          </button>
        </span>
      </div>

      <div className="card" style={{ paddingTop: 12, paddingBottom: 8 }}>
        {balErr ? (
          <div className="error-box">{balErr}</div>
        ) : (
          <table className="table">
            <thead>
              <tr>
                <th>Coin</th>
                <th>Side</th>
                <th className="num">Entry</th>
                <th className="num">Mark</th>
                <th className="num">Value</th>
                <th className="num">uPnL</th>
                <th className="num">%</th>
                <th>Source</th>
                <th>TP/SL</th>
                <th style={{ textAlign: "right" }}>Close</th>
                <th style={{ width: 26 }} />
              </tr>
            </thead>
            <tbody>
              {loading ? (
                <SkeletonRows rows={3} cols={cols} />
              ) : positions.length === 0 ? (
                <tr>
                  <td colSpan={cols}>
                    <EmptyState
                      icon={<TrendingUp size={18} />}
                      title="No open Perpetuals positions"
                      hint="caller + copy-trade orders land here"
                    />
                  </td>
                </tr>
              ) : (
                positions.map((p) => {
                  const f = rowFacts(p);
                  const isOpen = expanded === p.coin;
                  return (
                    <FragmentRow key={p.coin}>
                      <tr
                        onClick={() => setExpanded(isOpen ? null : p.coin)}
                        style={{ cursor: "pointer" }}
                      >
                        <td>
                          <span className="min-w-0">
                            <strong>{p.coin}</strong>
                            <span
                              className="block font-mono text-[10px] text-ink-4"
                              title="Position size (coin)"
                            >
                              {f.sizeAbs != null ? f.sizeAbs : p.szi}
                            </span>
                          </span>
                        </td>
                        <td>
                          <span className={`badge ${p.side === "long" ? "ok" : "fail"}`}>
                            {p.side}
                          </span>
                          {f.leverage != null && (
                            <span
                              className="block font-mono text-[10px] text-ink-4"
                              title="Leverage"
                            >
                              {f.leverage}×
                            </span>
                          )}
                        </td>
                        <td className="num">
                          {f.entry != null ? formatPerpPrice(f.entry) : "—"}
                        </td>
                        <td className="num">
                          {f.mark != null ? formatPerpPrice(f.mark) : "—"}
                        </td>
                        <td className="num">
                          {f.value != null ? fmtUsd(String(f.value)) : "—"}
                        </td>
                        <td className="num">
                          <span
                            className={
                              f.upnl == null ? "" : f.upnl > 0 ? "pos" : f.upnl < 0 ? "neg" : ""
                            }
                          >
                            {f.upnl == null
                              ? "—"
                              : `${f.upnl > 0 ? "+" : ""}${fmtUsd(String(f.upnl))}`}
                          </span>
                        </td>
                        <td className="num" title={f.roe ? "Return on equity (margin)" : "Unlevered return vs entry notional"}>
                          <PnlText pct={f.pct} digits={1} className="text-[11px]" />
                        </td>
                        <td>
                          <span
                            className="block font-mono text-[10px] text-ink-4 truncate"
                            style={{ maxWidth: 110 }}
                            title={f.source ?? undefined}
                          >
                            {sourceLabel(f.source)}
                          </span>
                        </td>
                        <td onClick={(e) => e.stopPropagation()} style={{ whiteSpace: "nowrap" }}>
                          <button
                            className="btn xs"
                            title="Attach reduce-only TP and/or SL triggers to this position"
                            onClick={() => setTpslFor(p)}
                          >
                            <Crosshair size={11} /> TP/SL
                          </button>
                        </td>
                        <td
                          onClick={(e) => e.stopPropagation()}
                          style={{ textAlign: "right", whiteSpace: "nowrap" }}
                        >
                          {[25, 50, 100].map((pc) => (
                            <button
                              key={pc}
                              className={`btn xs ${pc === 100 ? "danger" : ""}`}
                              style={{ marginLeft: 4 }}
                              title={`Close ${pc}% of the live position (reduce-only, type-to-confirm)`}
                              onClick={() => requestClose(p, pc)}
                            >
                              {pc}
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
                                  style={{ width: 110 }}
                                  inputMode="numeric"
                                  value={customText}
                                  onChange={(e) => setCustomText(e.target.value)}
                                  placeholder="33"
                                  aria-label="custom close percent"
                                />
                                <span className="mono text-[11px] text-ink-4">%</span>
                                <button
                                  className="btn sm danger"
                                  disabled={customPct() == null}
                                  title={
                                    customPct() == null
                                      ? "enter a percent in (0, 100]"
                                      : `Close ${customPct()}% of ${p.coin}`
                                  }
                                  onClick={() => {
                                    const pc = customPct();
                                    if (pc == null) return;
                                    requestClose(p, pc);
                                  }}
                                >
                                  <Scissors size={11} /> Close
                                </button>
                              </div>
                              <div className="ml-auto flex items-baseline gap-4 font-mono tabular-nums text-[11px]">
                                <span title="Liquidation price">
                                  <span className="text-ink-4">liq </span>
                                  <span className={f.liq != null ? "text-down" : "text-ink-3"}>
                                    {f.liq != null ? formatPerpPrice(f.liq) : "—"}
                                  </span>
                                </span>
                                <span title="Margin allocated to this position">
                                  <span className="text-ink-4">margin </span>
                                  <span className="text-ink-2">
                                    {f.margin != null ? fmtUsd(String(f.margin)) : "—"}
                                  </span>
                                </span>
                                <span title="Cumulative funding paid (+) / received (−) since open">
                                  <span className="text-ink-4">funding </span>
                                  <span
                                    className={
                                      f.funding == null || f.funding === 0
                                        ? "text-ink-3"
                                        : f.funding > 0
                                          ? "text-down"
                                          : "text-up"
                                    }
                                  >
                                    {f.funding != null
                                      ? `${f.funding > 0 ? "-" : "+"}${fmtUsd(
                                          String(Math.abs(f.funding)),
                                        )}`
                                      : "—"}
                                  </span>
                                </span>
                                <span title="Entry notional (size × entry)">
                                  <span className="text-ink-4">notional </span>
                                  <span className="text-ink-2">
                                    {f.sizeAbs != null && f.entry != null
                                      ? `$${compactNum(f.sizeAbs * f.entry)}`
                                      : "—"}
                                  </span>
                                </span>
                              </div>
                            </div>
                            <PerpChart
                              coin={p.coin}
                              entryPx={f.entry}
                              markPx={f.mark}
                              liqPx={f.liq}
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

      <ClosePositionDialog
        position={pendingClose?.position ?? null}
        paper={hl?.paper_mode ?? false}
        initialPercent={pendingClose?.percent ?? 100}
        onClose={() => {
          setPendingClose(null);
          setCustomText("");
        }}
        onDone={(msg) => {
          setNotice(msg);
          onReload();
        }}
      />
      <TpslDialog
        position={tpslFor}
        paper={hl?.paper_mode ?? false}
        onClose={() => setTpslFor(null)}
        onDone={(msg) => {
          setNotice(msg);
          onReload();
        }}
      />
    </>
  );
}

/** Keyed fragment helper so the (row, expanded-row) pair shares one key. */
function FragmentRow({ children }: { children: React.ReactNode }) {
  return <>{children}</>;
}
