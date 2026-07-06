// PositionChart — the row-expand chart surface: interval + MCap/Price
// toolbar over the ported TokenChart. Slim adaptation of the web's
// ChartPanel (frontend/modules/alpha-scanner/src/token/ChartPanel.tsx)
// without react-query / call markers: one history fetch per interval,
// lazy older-history paging via the `&before=` cursor, GeckoTerminal
// backfill fired once per mint, 5 s live tail poll inside TokenChart.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { fetchCandles, requestBackfill, type Candle } from "../data";
import { TokenChart, type ChartViewMode } from "./TokenChart";

const HISTORY_LIMIT = 500;

const INTERVALS: { secs: number; label: string }[] = [
  { secs: 1, label: "1s" },
  { secs: 15, label: "15s" },
  { secs: 60, label: "1m" },
  { secs: 300, label: "5m" },
  { secs: 900, label: "15m" },
  { secs: 3600, label: "1h" },
];

interface Props {
  address: string;
  symbol: string;
  /** Supply multiplier = mcap_now / price_now (MCap axis). */
  supply: number | null;
  /** Avg entry price (USD/token) → ENTRY line. */
  entryPriceUsd: number | null;
  /** Live price (USD/token) → NOW line seed. */
  currentPriceUsd: number | null;
  height?: number;
}

export function PositionChart({
  address,
  symbol,
  supply,
  entryPriceUsd,
  currentPriceUsd,
  height = 300,
}: Props) {
  const [interval_, setInterval_] = useState(60);
  const [viewMode, setViewMode] = useState<ChartViewMode>("mcap");
  const [base, setBase] = useState<Candle[] | null>(null);
  const [older, setOlder] = useState<Candle[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [hasMore, setHasMore] = useState(true);
  const inFlightBefore = useRef<string | null>(null);

  // Backfill once per mint (fire-and-forget; errors swallowed inside).
  const backfilledFor = useRef("");
  useEffect(() => {
    if (backfilledFor.current === address) return;
    backfilledFor.current = address;
    requestBackfill(address);
  }, [address]);

  // One history fetch per (mint, interval).
  useEffect(() => {
    let alive = true;
    setBase(null);
    setOlder([]);
    setErr(null);
    setHasMore(true);
    inFlightBefore.current = null;
    fetchCandles(address, interval_, HISTORY_LIMIT).then(
      (rows) => {
        if (!alive) return;
        setBase(rows);
      },
      (e) => {
        if (!alive) return;
        const msg = String(e);
        setErr(
          /403/.test(msg)
            ? "Chart history is a subscriber feature (the gateway answered 403)."
            : msg,
        );
      },
    );
    return () => {
      alive = false;
    };
  }, [address, interval_]);

  // Older-history paging (D2): prepend pages fetched via `&before=`.
  const onLoadMore = useCallback(
    (oldestUnixSecs: number) => {
      const beforeIso = new Date(oldestUnixSecs * 1000).toISOString();
      if (inFlightBefore.current === beforeIso) return;
      inFlightBefore.current = beforeIso;
      setLoadingMore(true);
      fetchCandles(address, interval_, HISTORY_LIMIT, beforeIso)
        .then(
          (rows) => {
            if (rows.length < HISTORY_LIMIT) setHasMore(false);
            if (rows.length > 0) setOlder((prev) => [...rows, ...prev]);
          },
          () => {
            // transient — allow a retrigger
            inFlightBefore.current = null;
          },
        )
        .finally(() => setLoadingMore(false));
    },
    [address, interval_],
  );

  // Merge base + older pages, de-duped by bucket ts (TokenChart also
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

  const isMcap = viewMode === "mcap" && supply != null && supply > 0;
  const mult = isMcap ? supply! : 1;
  const entryLevel =
    entryPriceUsd != null && entryPriceUsd > 0 ? entryPriceUsd * mult : null;
  const nowLevel =
    currentPriceUsd != null && currentPriceUsd > 0 ? currentPriceUsd * mult : null;

  return (
    <div>
      <div className="flex items-center gap-1 mb-1.5">
        <span className="hud-label" style={{ marginRight: 6 }}>
          {symbol} chart
        </span>
        {INTERVALS.map((iv) => (
          <button
            key={iv.secs}
            type="button"
            onClick={() => setInterval_(iv.secs)}
            className={`px-1.5 py-0.5 text-[10px] font-mono border rounded-sm transition-colors ${
              interval_ === iv.secs
                ? "border-accent/60 text-accent"
                : "border-line/15 text-ink-4 hover:text-ink-2"
            }`}
          >
            {iv.label}
          </button>
        ))}
        <span className="flex-1" />
        {(["mcap", "price"] as const).map((m) => (
          <button
            key={m}
            type="button"
            onClick={() => setViewMode(m)}
            className={`px-1.5 py-0.5 text-[10px] font-mono uppercase border rounded-sm transition-colors ${
              viewMode === m
                ? "border-accent/60 text-accent"
                : "border-line/15 text-ink-4 hover:text-ink-2"
            }`}
            title={m === "mcap" ? "Market-cap axis" : "Raw price axis"}
          >
            {m === "mcap" ? "MCap" : "Price"}
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
          No recorded candles for this token yet.
        </div>
      ) : (
        <TokenChart
          candles={candles}
          supply={isMcap ? supply : null}
          height={height}
          intervalSecs={interval_}
          address={address}
          viewMode={viewMode}
          entryMcap={entryLevel}
          currentMcap={nowLevel}
          liveEnabled
          onLoadMore={onLoadMore}
          isLoadingMore={loadingMore}
          hasMoreHistory={hasMore}
        />
      )}
    </div>
  );
}
