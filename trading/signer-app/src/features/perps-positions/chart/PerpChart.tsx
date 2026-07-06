// PerpChart — the row-expand chart surface for Perpetuals positions:
// interval toolbar over PerpCandleChart. Mirror of the Sol feature's
// PositionChart, adapted to the gateway's HL candle proxy
// (GET /api/hyperliquid/candles/{coin}?interval=&start=&end=):
// windowed start/end paging instead of the `&before=` cursor, no
// GeckoTerminal backfill, no MCap/Price toggle (perps = price axis),
// LIQ price line added. 5 s live tail poll inside PerpCandleChart.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  fetchPerpCandles,
  INTERVAL_SECS,
  type Candle,
  type PerpInterval,
} from "../data";
import { PerpCandleChart } from "./PerpCandleChart";

/** Bars per fetch — the gateway forwards to HL `candleSnapshot`
 *  (capped ~5000 server-side), 500 mirrors the Sol page size. */
const HISTORY_LIMIT = 500;

const INTERVALS: PerpInterval[] = ["1m", "5m", "15m", "1h", "4h", "1d"];

interface Props {
  coin: string;
  /** Avg entry price → ENTRY line. */
  entryPx: number | null;
  /** Live mark → NOW line seed. */
  markPx: number | null;
  /** Liquidation price → LIQ line. */
  liqPx: number | null;
  height?: number;
}

export function PerpChart({ coin, entryPx, markPx, liqPx, height = 300 }: Props) {
  const [interval_, setInterval_] = useState<PerpInterval>("15m");
  const [base, setBase] = useState<Candle[] | null>(null);
  const [older, setOlder] = useState<Candle[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [hasMore, setHasMore] = useState(true);
  const inFlightBefore = useRef<number | null>(null);

  // One history fetch per (coin, interval).
  useEffect(() => {
    let alive = true;
    setBase(null);
    setOlder([]);
    setErr(null);
    setHasMore(true);
    inFlightBefore.current = null;
    const end = Date.now();
    const start = end - HISTORY_LIMIT * INTERVAL_SECS[interval_] * 1000;
    fetchPerpCandles(coin, interval_, start, end).then(
      (rows) => {
        if (!alive) return;
        setBase(rows);
      },
      (e) => {
        if (!alive) return;
        setErr(String(e));
      },
    );
    return () => {
      alive = false;
    };
  }, [coin, interval_]);

  // Older-history paging (D2): prepend windows fetched before the
  // oldest rendered bucket.
  const onLoadMore = useCallback(
    (oldestUnixSecs: number) => {
      if (inFlightBefore.current === oldestUnixSecs) return;
      inFlightBefore.current = oldestUnixSecs;
      setLoadingMore(true);
      const end = oldestUnixSecs * 1000;
      const start = end - HISTORY_LIMIT * INTERVAL_SECS[interval_] * 1000;
      fetchPerpCandles(coin, interval_, start, end)
        .then(
          (rows) => {
            // Drop the boundary bucket (end is inclusive on HL's side).
            const fresh = rows.filter(
              (r) => Math.floor(Date.parse(r.ts) / 1000) < oldestUnixSecs,
            );
            if (fresh.length === 0) setHasMore(false);
            else setOlder((prev) => [...fresh, ...prev]);
          },
          () => {
            // transient — allow a retrigger
            inFlightBefore.current = null;
          },
        )
        .finally(() => setLoadingMore(false));
    },
    [coin, interval_],
  );

  // Merge base + older pages, de-duped by bucket ts (the chart also
  // de-dupes; keep the prop clean).
  const candles = useMemo<Candle[]>(() => {
    const b = base ?? [];
    if (older.length === 0) return b;
    const seen = new Set<string>();
    const out: Candle[] = [];
    for (const c of [...older, ...b]) {
      if (seen.has(c.ts)) continue;
      seen.add(c.ts);
      out.push(c);
    }
    return out;
  }, [base, older]);

  return (
    <div>
      <div className="flex items-center gap-1 mb-1.5">
        <span className="hud-label" style={{ marginRight: 6 }}>
          {coin} chart
        </span>
        {INTERVALS.map((iv) => (
          <button
            key={iv}
            type="button"
            onClick={() => setInterval_(iv)}
            className={`px-1.5 py-0.5 text-[10px] font-mono border rounded-sm transition-colors ${
              interval_ === iv
                ? "border-accent/60 text-accent"
                : "border-line/15 text-ink-4 hover:text-ink-2"
            }`}
          >
            {iv}
          </button>
        ))}
      </div>

      {err ? (
        <div
          className="flex items-center justify-center text-[11px] font-mono text-ink-4 border border-line/10"
          style={{ height }}
        >
          {err}
        </div>
      ) : base === null ? (
        <div
          className="flex items-center justify-center text-[11px] font-mono text-ink-4 animate-pulse border border-line/10"
          style={{ height }}
        >
          Loading candles…
        </div>
      ) : candles.length === 0 ? (
        <div
          className="flex items-center justify-center text-[11px] font-mono text-ink-4 border border-line/10"
          style={{ height }}
        >
          No candles for this market.
        </div>
      ) : (
        <PerpCandleChart
          candles={candles}
          height={height}
          interval={interval_}
          intervalSecs={INTERVAL_SECS[interval_]}
          coin={coin}
          entryPx={entryPx}
          markPx={markPx}
          liqPx={liqPx}
          liveEnabled
          onLoadMore={onLoadMore}
          isLoadingMore={loadingMore}
          hasMoreHistory={hasMore}
        />
      )}
    </div>
  );
}
