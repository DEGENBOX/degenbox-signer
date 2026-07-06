// usePolledCandles — keep the chart's right edge alive by POLLING the
// candle-history endpoint every 5 s (newest few buckets only) and
// folding them into the series via `series.update()`.
//
// WHY POLLING, NOT THE WEB'S NATS-WS TAPE (adaptation of the scanner's
// useLiveCandles): the gateway's `/ws` bridge authenticates with a
// `?token=` query param read from browser localStorage — but in this
// desktop app the JWT deliberately lives RUST-side only (resolve_auth +
// the `gateway_fetch` proxy). Exporting the bearer into the webview
// just for candles would break that boundary (and need a CSP wss:
// source + a fork of platform-fe's ws client whose auth-dead path
// hard-redirects to /login). Per the W3.1 dispatch, the sanctioned
// fallback is a 5 s poll of the same HTTP endpoint the seed uses —
// ~2 KB per tick through the existing authed proxy.
//
// Semantics preserved from useLiveCandles: per-bar wick clamp (D1),
// monotonic-time guard (never rewrite bars older than the series
// tail), display-unit mapping (× supply in MCap mode) identical to the
// seeded history.

import { useEffect, useRef } from "react";
import type { CandlestickData, ISeriesApi, UTCTimestamp } from "lightweight-charts";
import { fetchCandles } from "../data";
import { clampWick } from "./chartFormat";

const POLL_MS = 5_000;
/** Newest buckets refreshed per tick — covers the forming bar plus a
 *  late-sealing neighbour even on 1 s intervals. */
const TAIL_LIMIT = 4;

interface Args {
  address: string;
  /** Active interval in seconds — must match the seeded history. */
  intervalSecs: number;
  /** Display multiplier for MCap mode (null = raw price). */
  supply: number | null;
  /** Candlestick series to update; null until the chart mounts. */
  candleSeries: ISeriesApi<"Candlestick"> | null;
  /** Bucket-start unix (secs) of the newest bar already seeded — the
   *  monotonicity floor. Updated by the parent on every setData. */
  lastBarTimeSecs: number | null;
  /** Fresh close observed at the tail (display units) — feeds the NOW
   *  price line upstream. */
  onTailClose?: (close: number) => void;
  /** False while history is loading / chart unmounted. */
  enabled: boolean;
}

export function usePolledCandles({
  address,
  intervalSecs,
  supply,
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
  }, [lastBarTimeSecs, intervalSecs, address]);

  // Read through refs inside the poll loop so supply drift / callback
  // identity never restarts the interval.
  const supplyRef = useRef(supply);
  useEffect(() => {
    supplyRef.current = supply;
  }, [supply]);
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
        const page = await fetchCandles(address, intervalSecs, TAIL_LIMIT);
        if (!alive || page.length === 0) return;
        const sup = supplyRef.current;
        const toDisplay = (v: number) => (sup != null && sup > 0 ? v * sup : v);
        const bars = page
          .map((k) => {
            const t = Math.floor(Date.parse(k.ts) / 1000);
            const open = toDisplay(Number(k.open_usd));
            const high = toDisplay(Number(k.high_usd));
            const low = toDisplay(Number(k.low_usd));
            const close = toDisplay(Number(k.close_usd));
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
  }, [address, intervalSecs, candleSeries, enabled]);
}
