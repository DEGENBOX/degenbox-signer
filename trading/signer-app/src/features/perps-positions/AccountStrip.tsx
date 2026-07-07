// AccountStrip — the Perpetuals mirror of the Sol ClientsStrip: one
// compact hairline card for the HL master account (this device is the
// SOLE Perpetuals executor, so there is exactly one "client"):
// connection dot, master + agent wallets, account value, withdrawable,
// attributed unrealized PnL and the W2.2 daily/weekly/monthly
// realized-PnL windows (GET /api/hyperliquid/pnl/windows). A global
// sum rail closes the strip — THE one glow + corner-bracket focus of
// this screen (calm-pass decoration budget, docs/ui-idiom.md).
//
// Data ownership: the parent (PerpsPositions) owns the daemon status
// (`hl`, App-polled every 2 s) + the uPnL rollup; this component owns
// the strip-only feed (PnL windows) so a windows hiccup never blanks
// the table.

import { useEffect, useMemo, useState } from "react";
import { Link2 } from "lucide-react";
import type { HlStatus } from "../../ipc";
import { fetchPnlWindows, num, type HlPnlWindowsResponse, type UsdWindows } from "./data";
import { EmptyState, fmtUsd, shortAddr, timeAgo } from "../../components/ui";

const WINDOWS_POLL_MS = 30_000;

interface Props {
  /** Daemon HL status (App-polled). null = loading. */
  hl: HlStatus | null;
  /** Sum of unrealized PnL across the open-positions snapshot (USD). */
  upnlUsd: number | null;
}

function signed(v: number | null): string {
  if (v == null) return "—";
  const sign = v > 0 ? "+" : "";
  return `${sign}${fmtUsd(String(v))}`;
}

function toneCls(v: number | null): string {
  if (v == null || v === 0) return "text-ink-3";
  return v > 0 ? "text-up" : "text-down";
}

const WINDOW_KEYS = ["d1", "d7", "d30"] as const;
const WINDOW_PREFIX = ["d ", "w ", "m "];

function windowVal(w: UsdWindows | null, k: (typeof WINDOW_KEYS)[number]): number | null {
  return w ? num(w[k]) : null;
}

export function AccountStrip({ hl, upnlUsd }: Props) {
  const [windows, setWindows] = useState<HlPnlWindowsResponse | null>(null);

  // Realized-PnL windows (gateway, W2.2).
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

  const account = hl?.account_address ?? null;

  /** Windows row for THIS master wallet (fall back to totals when the
   *  per-wallet row hasn't resolved — single-executor, same numbers). */
  const accountWindows = useMemo<UsdWindows | null>(() => {
    if (!windows) return null;
    if (account) {
      const row = windows.clients.find(
        (c) => c.wallet.toLowerCase() === account.toLowerCase(),
      );
      if (row) return row.realized_usd;
    }
    return windows.totals.realized_usd;
  }, [windows, account]);

  const totalsWindows = windows?.totals.realized_usd ?? null;

  const paired = hl?.paired ?? false;
  const withdrawable = num(hl?.balance.withdrawable_usd ?? null);
  // UNIFIED account: HL trades ONE balance (spot backs perp automatically),
  // greys out the spot↔perp transfer, and reports perp accountValue as $0
  // with the money in spot. Show a SINGLE truthful account value and never
  // the separated perp/spot split. SEPARATED account: keep the co-equal
  // perp + spot read (spot is a distinct wallet needing a transfer).
  const isUnified = hl?.balance.is_unified ?? false;
  const perpUsd = num(hl?.balance.account_value_usd ?? null);
  const spotUsd = num(hl?.balance.spot_usdc ?? null);
  const unifiedUsd = num(hl?.balance.unified_value_usd ?? null);
  // The single "equity" figure the strip leads with:
  //   unified   → the combined value HL shows the user (perp + spot)
  //   separated → the perp equity (spot rendered separately)
  const balUsd = isUnified ? unifiedUsd : perpUsd;
  const hasIdleSpot = spotUsd != null && spotUsd > 0.01;

  const dot = !hl
    ? "grey"
    : hl.conn === "ready"
      ? "green"
      : hl.conn === "paused" || hl.conn === "connecting"
        ? "amber"
        : hl.conn === "error"
          ? "red"
          : "grey";

  return (
    <div className="mb-3.5">
      <div className="shell-section-head">
        <span className="section-num">01</span>
        <span className="shell-section-title">Account</span>
        <span className="hud-label brackets">{paired ? "1" : "–"}</span>
        {hl?.paper_mode && <span className="badge warn">paper</span>}
      </div>

      {!paired ? (
        <div className="border border-line/10">
          <EmptyState
            icon={<Link2 size={18} />}
            title={hl === null ? "Loading account…" : "Not paired yet"}
            hint={hl === null ? undefined : "pair this device on the Bots tab"}
          />
        </div>
      ) : (
        <div
          className="grid gap-px bg-line/10 border border-line/10"
          style={{ gridTemplateColumns: "repeat(auto-fit, minmax(230px, 1fr))" }}
        >
          <div className="bg-canvas px-3 py-2.5">
            <div className="flex items-baseline gap-2 min-w-0">
              <span className={`status-dot ${dot}`} style={{ alignSelf: "center" }} />
              <span className="text-[12px] font-medium text-ink-1 truncate">
                Master account
              </span>
              <span className="ml-auto font-mono text-[10px] text-ink-4">
                {account ? shortAddr(account, 6, 4) : "—"}
              </span>
            </div>
            <div
              className="mt-0.5 text-[10px] font-mono text-ink-4 uppercase tracking-wider truncate"
              title={hl?.agent_address ?? undefined}
            >
              {hl?.agent_address
                ? `agent ${shortAddr(hl.agent_address, 6, 4)}`
                : "no agent wallet"}
            </div>
            <div className="mt-1.5 flex items-baseline gap-3 font-mono tabular-nums text-[11px]">
              <span
                title={`${
                  isUnified
                    ? "Account value (unified — spot backs perp automatically)"
                    : "Account value (perp equity)"
                }${
                  hl?.balance.fetched_at ? ` · updated ${timeAgo(hl.balance.fetched_at)}` : ""
                }`}
              >
                <span className="text-ink-4">{isUnified ? "balance " : "equity "}</span>
                <span
                  className={
                    !isUnified && balUsd === 0 && hasIdleSpot
                      ? "text-amber-300"
                      : "text-ink-1"
                  }
                >
                  {balUsd != null ? fmtUsd(String(balUsd)) : "—"}
                </span>
              </span>
              {/* Separated account only: spot is a distinct wallet. For a
                  unified account the spot USDC is already IN `balance`, so a
                  second "spot" cell would double-count + mislead. */}
              {!isUnified && (
                <span title="Spot USDC (separate wallet from perp)">
                  <span className="text-ink-4">spot </span>
                  <span className={hasIdleSpot ? "text-amber-300" : "text-ink-2"}>
                    {spotUsd != null ? fmtUsd(String(spotUsd)) : "—"}
                  </span>
                </span>
              )}
              <span title="Withdrawable USDC">
                <span className="text-ink-4">free </span>
                <span className="text-ink-2">
                  {withdrawable != null ? fmtUsd(String(withdrawable)) : "—"}
                </span>
              </span>
              <span title="Unrealized PnL (open positions)">
                <span className="text-ink-4">upnl </span>
                <span className={toneCls(upnlUsd)}>{signed(upnlUsd)}</span>
              </span>
            </div>
            <div
              className="mt-1 flex items-baseline gap-3 font-mono tabular-nums text-[11px]"
              title="Realized PnL (USD): 1d / 7d / 30d"
            >
              {WINDOW_KEYS.map((k, i) => {
                const v = windowVal(accountWindows, k);
                return (
                  <span key={k}>
                    <span className="text-ink-4">{WINDOW_PREFIX[i]}</span>
                    <span className={toneCls(v)}>{signed(v)}</span>
                  </span>
                );
              })}
            </div>
          </div>
        </div>
      )}

      {/* Global sum rail — THE one glow + corner-bracket focus on this
          screen (decoration budget: signature element only). */}
      <div className="corners glow-accent border border-accent/25 bg-card/75 px-3 py-2 flex items-baseline gap-5 font-mono tabular-nums text-[12px] mt-px">
        <span className="hud-label brackets">Total</span>
        <span title="Account value (equity)">
          <span className="text-ink-4">equity </span>
          <span className="text-ink-1">{balUsd != null ? fmtUsd(String(balUsd)) : "—"}</span>
        </span>
        <span title="Unrealized PnL across all open positions">
          <span className="text-ink-4">upnl </span>
          <span className={`glow-text ${toneCls(upnlUsd)}`}>{signed(upnlUsd)}</span>
        </span>
        <span className="ml-auto flex items-baseline gap-4" title="Realized PnL: 1d / 7d / 30d">
          {WINDOW_KEYS.map((k, i) => {
            const v = windowVal(totalsWindows, k);
            return (
              <span key={k}>
                <span className="text-ink-4">{WINDOW_PREFIX[i]}</span>
                <span className={toneCls(v)}>{signed(v)}</span>
              </span>
            );
          })}
        </span>
      </div>
    </div>
  );
}
