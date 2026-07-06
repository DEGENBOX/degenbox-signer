// TokenChart — candlestick + volume chart on TradingView
// `lightweight-charts` v5 for the Positions row-expand.
//
// COPIED + ADAPTED (W3.1, bot-redesign) from
// frontend/modules/alpha-scanner/src/token/TokenChart.tsx (module
// boundaries forbid cross-imports; the scanner files are hot). Diffs
// vs. the source, kept deliberately small so fixes port both ways:
//   · live feed: `useLiveCandles` (NATS-WS tape) → `usePolledCandles`
//     (5 s HTTP poll through the Rust-side authed proxy — see that
//     file's header for the WS-auth rationale);
//   · call markers / CallMarkersOverlay / imperative handle removed
//     (no caller context on a position chart); price lines reduced to
//     ENTRY (avg cost basis) + the solid NOW line;
//   · `Candle` type comes from ../data (same wire shape).
// All the hard-won seed logic is verbatim: D1 wick clamp, series spike
// guard, C1 gated fitContent, C2 percentile autoscale band, C3 minMove
// derivation, D2 older-history prepend anchoring, D7 token-switch
// clear, crosshair OHLCV tooltip.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  CandlestickSeries,
  HistogramSeries,
  CrosshairMode,
  LineStyle,
  createChart,
  type AutoscaleInfo,
  type CandlestickData,
  type HistogramData,
  type IChartApi,
  type IPriceLine,
  type ISeriesApi,
  type LogicalRange,
  type UTCTimestamp,
} from "lightweight-charts";
import { useChartColors } from "@degenbox/ui";
import type { Candle } from "../data";
import { usePolledCandles } from "./usePolledCandles";
import {
  clampWick,
  clampSpikes,
  formatRawPrice,
  lighten,
  type TimedBar,
} from "./chartFormat";

export type ChartViewMode = "mcap" | "price";

// ── MCap formatting (mirrors scanner compactMcap) ─────────────────
function compactMcap(v: number): string {
  if (!Number.isFinite(v) || v <= 0) return "—";
  if (v < 1_000) return `$${v.toFixed(0)}`;
  if (v < 1_000_000) return `$${(v / 1_000).toFixed(v < 10_000 ? 1 : 0)}K`;
  if (v < 1_000_000_000) return `$${(v / 1_000_000).toFixed(v < 10_000_000 ? 2 : 1)}M`;
  return `$${(v / 1_000_000_000).toFixed(2)}B`;
}

export interface TokenChartProps {
  candles: Candle[];
  supply: number | null;
  height: number;
  /** Active interval in seconds — drives live bucketing. */
  intervalSecs: number;
  /** Token address — used by the live tail poll. */
  address: string;
  /** Mcap vs raw-price axis. Defaults to mcap (memecoin default). */
  viewMode?: ChartViewMode;
  /** Avg-entry level (display units) → dashed ENTRY line. */
  entryMcap?: number | null;
  /** Live current MCap (or price in price-mode) for the solid NOW line. */
  currentMcap?: number | null;
  /** Pause live updates (e.g. while history is still loading). */
  liveEnabled?: boolean;
  /** Older-history paging: called when the user pans near the left edge.
   *  Receives the oldest bucket ts (unix seconds) currently rendered. */
  onLoadMore?: (oldestUnixSecs: number) => void;
  isLoadingMore?: boolean;
  hasMoreHistory?: boolean;
}

/** ISO/Date → lightweight-charts UTCTimestamp (unix seconds). */
function toUnixSecs(ts: string | Date): number {
  const ms = ts instanceof Date ? ts.getTime() : Date.parse(ts);
  return Math.floor(ms / 1000);
}

export function TokenChart({
  candles,
  supply,
  height,
  intervalSecs,
  address,
  viewMode = "mcap",
  entryMcap,
  currentMcap,
  liveEnabled = true,
  onLoadMore,
  isLoadingMore = false,
  hasMoreHistory = true,
}: TokenChartProps) {
  const colors = useChartColors();
  const crosshairColor = useMemo(() => lighten(colors.line, 0.45), [colors.line]);
  const nowLineColor = useMemo(() => lighten(colors.ink, 0.5), [colors.ink]);
  const containerRef = useRef<HTMLDivElement | null>(null);
  const chartRef = useRef<IChartApi | null>(null);
  const candleRef = useRef<ISeriesApi<"Candlestick"> | null>(null);
  const volumeRef = useRef<ISeriesApi<"Histogram"> | null>(null);
  const priceLinesRef = useRef<IPriceLine[]>([]);
  const nowLineRef = useRef<IPriceLine | null>(null);
  // Re-render once the chart mounts so usePolledCandles receives the
  // series (refs alone wouldn't re-run the hook's effect).
  const [, setMounted] = useState(false);

  const isMcap = viewMode === "mcap" && supply != null && supply > 0;

  // Forces the live updater to reset its tail whenever we re-seed the
  // series via setData (interval switch / older paging / fresh).
  const [lastBarTimeSecs, setLastBarTimeSecs] = useState<number | null>(null);
  // Freshest tail close observed by the poll (display units) — keeps
  // the NOW line moving between parent refreshes.
  const [polledNow, setPolledNow] = useState<number | null>(null);
  // Crosshair tooltip state (O/H/L/C/V of the hovered bar).
  const [hover, setHover] = useState<{
    x: number;
    o: number;
    h: number;
    l: number;
    c: number;
    v: number;
    time: number;
  } | null>(null);

  const toDisplay = useMemo(
    () => (price: number) => (isMcap ? price * supply! : price),
    [isMcap, supply],
  );

  const fmtValue = useCallback(
    (v: number) => (isMcap ? compactMcap(v) : formatRawPrice(v)),
    [isMcap],
  );

  // ── C1: gate fitContent() to genuine re-seeds ────────────────────
  const reseedRef = useRef(true);
  // D2: rendered bar count → detect older-history prepends.
  const prevLenRef = useRef(0);

  useEffect(() => {
    reseedRef.current = true;
    prevLenRef.current = 0;
    setPolledNow(null);
  }, [intervalSecs, address, viewMode]);

  // ── D7: clear the series on token change ─────────────────────────
  useEffect(() => {
    candleRef.current?.setData([]);
    volumeRef.current?.setData([]);
    // address is the only dep on purpose — interval re-seeds keep the
    // old bars visible until the new page lands (no flash, same token).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [address]);

  // ── Mount the chart once ──────────────────────────────────────────
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;

    const chart = createChart(el, {
      autoSize: true,
      layout: {
        background: { color: "transparent" },
        textColor: colors.ink,
        fontFamily: "ui-monospace, SFMono-Regular, monospace",
        fontSize: 10,
        attributionLogo: false,
      },
      grid: {
        vertLines: { visible: false },
        horzLines: { visible: false },
      },
      crosshair: {
        mode: CrosshairMode.Normal,
        vertLine: {
          color: crosshairColor,
          width: 1,
          style: LineStyle.Dashed,
          labelVisible: true,
          labelBackgroundColor: colors.accent,
        },
        horzLine: {
          color: crosshairColor,
          width: 1,
          style: LineStyle.Dashed,
          labelVisible: true,
          labelBackgroundColor: colors.accent,
        },
      },
      rightPriceScale: {
        borderColor: colors.line,
        scaleMargins: { top: 0.08, bottom: 0.26 },
      },
      timeScale: {
        borderColor: colors.line,
        timeVisible: true,
        secondsVisible: false,
        rightOffset: 4,
      },
      handleScroll: true,
      handleScale: true,
      localization: {
        priceFormatter: (p: number) => fmtValueRef.current(p),
      },
    });
    chartRef.current = chart;

    const candleSeries = chart.addSeries(CandlestickSeries, {
      upColor: colors.up,
      downColor: colors.down,
      borderVisible: false,
      wickUpColor: colors.up,
      wickDownColor: colors.down,
      priceLineVisible: false,
      lastValueVisible: true,
      // C3: data-derived minMove — recomputed in the seed effect.
      priceFormat: {
        type: "custom",
        minMove: 0.00000001,
        formatter: (p: number) => fmtValueRef.current(p),
      },
      // C2: clamp the AUTO scale to the percentile band (seed effect).
      autoscaleInfoProvider: (orig: () => AutoscaleInfo | null): AutoscaleInfo | null => {
        const band = autoscaleBandRef.current;
        if (!band) return orig();
        return { priceRange: { minValue: band.min, maxValue: band.max } };
      },
    });
    candleRef.current = candleSeries;

    const volumeSeries = chart.addSeries(HistogramSeries, {
      priceFormat: { type: "volume" },
      priceScaleId: "vol",
      color: colors.up,
    });
    volumeSeries.priceScale().applyOptions({
      scaleMargins: { top: 0.82, bottom: 0 },
    });
    volumeRef.current = volumeSeries;
    setMounted(true);

    // Crosshair tooltip — read OHLCV off the hovered point.
    const onMove: Parameters<IChartApi["subscribeCrosshairMove"]>[0] = (param) => {
      if (!param.point || param.time === undefined || !candleRef.current) {
        setHover(null);
        return;
      }
      const cd = param.seriesData.get(candleRef.current) as CandlestickData | undefined;
      const vd = volumeRef.current
        ? (param.seriesData.get(volumeRef.current) as HistogramData | undefined)
        : undefined;
      if (!cd) {
        setHover(null);
        return;
      }
      setHover({
        x: param.point.x,
        o: cd.open,
        h: cd.high,
        l: cd.low,
        c: cd.close,
        v: vd?.value ?? 0,
        time: Number(param.time),
      });
    };
    chart.subscribeCrosshairMove(onMove);

    // ── D2: lazy older-history paging near the left edge ─────────────
    const onLogicalRange = (range: LogicalRange | null) => {
      if (!range) return;
      const cb = onLoadMoreRef.current;
      if (!cb || isLoadingMoreRef.current || !hasMoreHistoryRef.current) return;
      if (reseedRef.current) return;
      if (range.from <= 5) {
        const oldest = oldestUnixRef.current;
        if (oldest != null) cb(oldest);
      }
    };
    chart.timeScale().subscribeVisibleLogicalRangeChange(onLogicalRange);

    return () => {
      chart.unsubscribeCrosshairMove(onMove);
      chart.timeScale().unsubscribeVisibleLogicalRangeChange(onLogicalRange);
      chart.remove();
      chartRef.current = null;
      candleRef.current = null;
      volumeRef.current = null;
      priceLinesRef.current = [];
      nowLineRef.current = null;
    };
    // Mount once; theme/supply/view changes are applied via applyOptions
    // below. Live values are read through refs so the handlers stay stable.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Refs the chart-mount closures read so they never go stale.
  const fmtValueRef = useRef(fmtValue);
  const onLoadMoreRef = useRef(onLoadMore);
  const isLoadingMoreRef = useRef(isLoadingMore);
  const hasMoreHistoryRef = useRef(hasMoreHistory);
  const oldestUnixRef = useRef<number | null>(null);
  const currentMcapRef = useRef<number | null>(currentMcap ?? null);
  const autoscaleBandRef = useRef<{ min: number; max: number } | null>(null);
  useEffect(() => {
    fmtValueRef.current = fmtValue;
  }, [fmtValue]);
  useEffect(() => {
    const live = polledNow ?? currentMcap ?? null;
    currentMcapRef.current = live;
    // Keep the C2 band ceiling ahead of a genuine live high between
    // re-seeds, capped at 1.5× so a lone spike can't re-blow the scale.
    const band = autoscaleBandRef.current;
    if (band && live != null && live > band.max && live <= band.max * 1.5) {
      band.max = live;
    }
  }, [currentMcap, polledNow]);
  useEffect(() => {
    onLoadMoreRef.current = onLoadMore;
    isLoadingMoreRef.current = isLoadingMore;
    hasMoreHistoryRef.current = hasMoreHistory;
  }, [onLoadMore, isLoadingMore, hasMoreHistory]);

  // ── Re-apply theme + price formatter when colors / view change ────
  useEffect(() => {
    const chart = chartRef.current;
    const candle = candleRef.current;
    if (!chart || !candle) return;
    chart.applyOptions({
      layout: { textColor: colors.ink },
      crosshair: {
        vertLine: { color: crosshairColor, labelBackgroundColor: colors.accent },
        horzLine: { color: crosshairColor, labelBackgroundColor: colors.accent },
      },
      rightPriceScale: { borderColor: colors.line },
      timeScale: { borderColor: colors.line },
      localization: {
        priceFormatter: (p: number) => fmtValueRef.current(p),
      },
    });
    candle.applyOptions({
      upColor: colors.up,
      downColor: colors.down,
      wickUpColor: colors.up,
      wickDownColor: colors.down,
    });
  }, [colors, crosshairColor, isMcap]);

  // ── Seed history into the series ──────────────────────────────────
  useEffect(() => {
    const candle = candleRef.current;
    const volume = volumeRef.current;
    if (!candle) return;

    // De-dupe → TimedBar[], clamping wicks on post-toDisplay() values.
    const byTime = new Map<number, TimedBar>();
    for (const k of candles) {
      const t = toUnixSecs(k.ts);
      if (!Number.isFinite(t)) continue;
      const open = toDisplay(Number(k.open_usd));
      const high = toDisplay(Number(k.high_usd));
      const low = toDisplay(Number(k.low_usd));
      const close = toDisplay(Number(k.close_usd));
      if (![open, high, low, close].every(Number.isFinite)) continue;
      const clamped = clampWick({ open, high, low, close });
      const vol = Number(k.volume_usd);
      byTime.set(t, {
        time: t,
        ...clamped,
        volume: Number.isFinite(vol) && vol > 0 ? vol : 0,
      });
    }
    const sorted = [...byTime.values()].sort((a, b) => a.time - b.time);
    const bars = clampSpikes(sorted);

    const candleData: CandlestickData[] = bars.map((b) => ({
      time: b.time as UTCTimestamp,
      open: b.open,
      high: b.high,
      low: b.low,
      close: b.close,
    }));
    const volData: HistogramData[] = bars.map((b) => ({
      time: b.time as UTCTimestamp,
      value: b.volume,
      color: b.close >= b.open ? colors.up : colors.down,
    }));

    // ── C3: derive minMove from the smallest close magnitude ─────────
    if (candleData.length > 0) {
      let minClose = Infinity;
      for (const c of candleData) if (c.close > 0 && c.close < minClose) minClose = c.close;
      if (Number.isFinite(minClose) && minClose > 0) {
        const exp = Math.floor(Math.log10(minClose));
        const minMove = Math.pow(10, Math.min(exp - 2, 0));
        candle.applyOptions({
          priceFormat: {
            type: "custom",
            minMove: minMove > 0 ? minMove : 0.00000001,
            formatter: (p: number) => fmtValueRef.current(p),
          },
        });
      }
    }

    // ── C2: robust percentile autoscale band ─────────────────────────
    {
      const lows: number[] = [];
      const highs: number[] = [];
      for (const c of candleData) {
        if (c.low > 0) lows.push(c.low);
        if (c.high > 0) highs.push(c.high);
      }
      if (lows.length > 0 && highs.length > 0) {
        lows.sort((a, b) => a - b);
        highs.sort((a, b) => a - b);
        const pct = (arr: number[], q: number) =>
          arr[Math.min(arr.length - 1, Math.max(0, Math.round((arr.length - 1) * q)))]!;
        const trim = candleData.length >= 50 ? 0.01 : 0;
        const loB = pct(lows, trim);
        const hiB = pct(highs, 1 - trim);
        const live = currentMcapRef.current ?? 0;
        let max = hiB * 1.08;
        if (live > max && live <= max * 1.5) max = live * 1.02;
        autoscaleBandRef.current = {
          min: Math.max(0, loB * 0.92),
          max: max > loB ? max : hiB * 1.08,
        };
      } else {
        autoscaleBandRef.current = null;
      }
    }

    const chart = chartRef.current;
    // ── D2: detect an older-history prepend to keep the user anchored.
    const prevLen = prevLenRef.current;
    const isPrepend = !reseedRef.current && prevLen > 0 && candleData.length > prevLen;
    const rangeBefore =
      isPrepend && chart ? chart.timeScale().getVisibleLogicalRange() : null;
    const delta = candleData.length - prevLen;

    candle.setData(candleData);
    if (volume) volume.setData(volData);
    prevLenRef.current = candleData.length;

    oldestUnixRef.current = sorted.length > 0 ? sorted[0]!.time : null;
    const last = sorted.length > 0 ? sorted[sorted.length - 1]!.time : null;
    setLastBarTimeSecs(last);

    // ── C1 / D2: frame ONLY on a genuine re-seed; on a prepend shift
    // the visible window by the prepended count; otherwise do nothing.
    if (chart && candleData.length > 0) {
      if (isPrepend && rangeBefore) {
        requestAnimationFrame(() => {
          try {
            chart.timeScale().setVisibleLogicalRange({
              from: rangeBefore.from + delta,
              to: rangeBefore.to + delta,
            });
          } catch {
            /* range no longer valid */
          }
        });
      } else if (reseedRef.current) {
        reseedRef.current = false;
        requestAnimationFrame(() => {
          try {
            chart.timeScale().fitContent();
          } catch {
            /* chart removed mid-frame */
          }
        });
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [candles, toDisplay, intervalSecs, colors.up, colors.down]);

  // ── Price line: ENTRY (avg cost basis) ─────────────────────────────
  useEffect(() => {
    const candle = candleRef.current;
    if (!candle) return;
    for (const pl of priceLinesRef.current) {
      try {
        candle.removePriceLine(pl);
      } catch {
        /* line already gone on series re-seed */
      }
    }
    priceLinesRef.current = [];
    if (entryMcap != null && entryMcap > 0) {
      priceLinesRef.current.push(
        candle.createPriceLine({
          price: entryMcap,
          color: colors.accent,
          lineWidth: 1,
          lineStyle: LineStyle.Dashed,
          axisLabelVisible: true,
          title: `ENTRY ${fmtValue(entryMcap)}`,
        }),
      );
    }
  }, [entryMcap, fmtValue, colors.accent, candles]);

  // ── Solid "NOW" current-value line ─────────────────────────────────
  useEffect(() => {
    const candle = candleRef.current;
    if (!candle) return;
    if (nowLineRef.current) {
      try {
        candle.removePriceLine(nowLineRef.current);
      } catch {
        /* gone on re-seed */
      }
      nowLineRef.current = null;
    }
    const now = polledNow ?? currentMcap ?? null;
    if (now == null || !(now > 0)) return;
    nowLineRef.current = candle.createPriceLine({
      price: now,
      color: nowLineColor,
      lineWidth: 1,
      lineStyle: LineStyle.Solid,
      axisLabelVisible: true,
      title: `NOW ${fmtValue(now)}`,
    });
  }, [currentMcap, polledNow, fmtValue, nowLineColor, candles]);

  // ── Live right edge: 5 s tail poll (WS-auth rationale in the hook) ─
  usePolledCandles({
    address,
    intervalSecs,
    supply: isMcap ? supply : null,
    candleSeries: candleRef.current,
    lastBarTimeSecs,
    onTailClose: setPolledNow,
    enabled: liveEnabled,
  });

  return (
    <div className="relative w-full" style={{ height }}>
      <div ref={containerRef} className="absolute inset-0" />
      {isLoadingMore && (
        <div className="pointer-events-none absolute left-2 bottom-8 z-10">
          <span className="bg-card/90 border border-line/20 rounded px-2 py-0.5 text-[9px] font-mono text-ink-3 backdrop-blur-sm">
            Loading more…
          </span>
        </div>
      )}
      {hover && (
        <div className="pointer-events-none absolute left-2 top-1 z-10">
          <div className="bg-card/95 border border-line/20 rounded px-2 py-1 text-[9px] font-mono tabular-nums flex items-center gap-2.5 backdrop-blur-sm">
            <span className="text-ink-4">O</span>
            <span className="text-ink-2">{fmtValue(hover.o)}</span>
            <span className="text-ink-4">H</span>
            <span className="text-up">{fmtValue(hover.h)}</span>
            <span className="text-ink-4">L</span>
            <span className="text-down">{fmtValue(hover.l)}</span>
            <span className="text-ink-4">C</span>
            <span className={hover.c >= hover.o ? "text-up" : "text-down"}>
              {fmtValue(hover.c)}
            </span>
            <span className="text-ink-4">V</span>
            <span className="text-ink-2">{compactMcap(hover.v)}</span>
          </div>
        </div>
      )}
    </div>
  );
}
