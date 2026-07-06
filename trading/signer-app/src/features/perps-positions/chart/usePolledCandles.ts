// usePolledCandles (perps) — keep the chart's right edge alive by
// POLLING the gateway's HL candle proxy every 5 s (newest few buckets
// only) and folding them into the series via `series.update()`.
//
// COPIED + GENERALISED (W4.1) from
// features/positions/chart/usePolledCandles.ts — that copy is wired to
// the Sol alpha-history endpoint (mint + interval_secs) and is owned by
// the Sol Positions feature, so per the wave rules it is copied here
// instead of modified. Diffs vs. the source, kept deliberately small:
//   · data source: `fetchCandles(mint, secs, limit)` →
//     `fetchPerpCandles(coin, interval, start, end)` with a tail window
//     of TAIL_LIMIT buckets ending now;
//   · no supply/MCap display mapping (perps charts are price-axis only).
// Semantics preserved: per-bar wick clamp (D1), monotonic-time guard
// (never rewrite bars older than the series tail).

import { useEffect, useRef } from "react";
import type { CandlestickData, ISeriesApi, UTCTimestamp } from "lightweight-charts";
import { fetchPerpCandles, INTERVAL_SECS, type PerpInterval } from "../data";
import { clampWick } from "../../positions/chart/chartFormat";

const POLL_MS = 5_000;
/** Newest buckets refreshed per tick — covers the forming bar plus a
 *  late-sealing neighbour. */
const TAIL_LIMIT = 4;

interface Args {
  coin: string;
  /** Active interval — must match the seeded history. */
  interval: PerpInterval;
  /** Candlestick series to update; null until the chart mounts. */
  candleSeries: ISeriesApi<"Candlestick"> | null;
  /** Bucket-start unix (secs) of the newest bar already seeded — the
   *  monotonicity floor. Updated by the parent on every setData. */
  lastBarTimeSecs: number | null;
  /** Fresh close observed at the tail — feeds the NOW price line. */
  onTailClose?: (close: number) => void;
  /** False while history is loading / chart unmounted. */
  enabled: boolean;
}

export function usePolledCandles({
  coin,
  interval,
  candleSeries,
  lastBarTimeSecs,
  onTailClose,
  enabled,
}: Args): void {
  // Newest bar time the series holds — series.update() requires
  // monotonically non-decreasing times.
  const tailRef = useRef<number | null>(lastBarTimeSecs);
  useEffect(() => {
    tailRef.current = lastBarTimeSecs;
  }, [lastBarTimeSecs, interval, coin]);

  const onTailCloseRef = useRef(onTailClose);
  useEffect(() => {
    onTailCloseRef.current = onTailClose;
  }, [onTailClose]);

  useEffect(() => {
    if (!enabled || !candleSeries) return;
    let alive = true;
    let inFlight = false;

    const tick = async () => {
      if (inFlight) return;
      inFlight = true;
      try {
        const end = Date.now();
        const start = end - TAIL_LIMIT * INTERVAL_SECS[interval] * 1000;
        const page = await fetchPerpCandles(coin, interval, start, end);
        if (!alive || page.length === 0) return;
        const bars = page
          .map((k) => {
            const t = Math.floor(Date.parse(k.ts) / 1000);
            const open = Number(k.open_usd);
            const high = Number(k.high_usd);
            const low = Number(k.low_usd);
            const close = Number(k.close_usd);
            if (!Number.isFinite(t) || ![open, high, low, close].every(Number.isFinite)) {
              return null;
            }
            return { time: t, ...clampWick({ open, high, low, close }) };
          })
          .filter((b): b is { time: number } & ReturnType<typeof clampWick> => b !== null)
          .sort((a, b) => a.time - b.time);

        for (const b of bars) {
          const floor = tailRef.current;
          // Never rewrite bars older than the series tail.
          if (floor != null && b.time < floor) continue;
          const candle: CandlestickData = {
            time: b.time as UTCTimestamp,
            open: b.open,
            high: b.high,
            low: b.low,
            close: b.close,
          };
          try {
            candleSeries.update(candle);
            tailRef.current = b.time;
          } catch {
            // out-of-order vs. the series — drop, next tick recovers
          }
        }
        const newest = bars[bars.length - 1];
        if (newest && newest.close > 0) onTailCloseRef.current?.(newest.close);
      } catch {
        // transient fetch error — keep polling
      } finally {
        inFlight = false;
      }
    };

    tick();
    const id = setInterval(tick, POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [coin, interval, candleSeries, enabled]);
}
