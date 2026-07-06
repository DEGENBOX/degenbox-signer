// chartFormat — shared chart helpers used by BOTH the static seed
// (TokenChart.tsx) and the live tail updater (usePolledCandles.ts) so
// the two paths stay byte-identical.
//
// COPIED (W3.1, bot-redesign) from
// frontend/modules/alpha-scanner/src/token/chartFormat.ts — module
// boundaries forbid cross-imports and the scanner files are hot.
// Keep changes here in sync manually if the source evolves.

// ── D1: two-stage wick clamp ────────────────────────────────────────
// Stage 1 hard-caps a wick at 5× the close (catches garbage prints /
// flash crashes); stage 2 tightens to a 200% tolerance (allows up to a
// genuine 3× pump but no more). Without this one corrupt high flattens
// every other candle to a hairline. Ratio-based, so it is unit-agnostic
// and applies identically to MCap and raw-price values.
const HARD_CAP_MULTIPLE = 5;
const WICK_TOLERANCE = 2.0; // 200% → effective 3× cap

export interface OHLC {
  open: number;
  high: number;
  low: number;
  close: number;
}

/** One bar carrying its bucket-start time (unix seconds) + display OHLCV.
 *  The shared currency between the seed path and the gap-filler. */
export interface TimedBar extends OHLC {
  time: number; // unix seconds (bucket start)
  volume: number;
}

// ── Trade-driven rendering (Axiom-style) ────────────────────────────
// We deliberately do NOT synthesize flat carry-forward candles for empty
// buckets (the former D6 `fillGaps` / D8 live advancer). lightweight-charts
// lays bars out by INDEX, not wall-clock, so omitting empty buckets makes
// the real candles sit adjacent and the chart reads cleanly horizontally —
// each candle = real trade activity (Axiom/GMGN behaviour) instead of long
// flat runs that drown out the signal. A new bar still opens at the prior
// close (D3 continuity), so a post-silence trade renders as a real up/down
// candle rather than a detached doji.

// ── Series-aware spike guard (the wick guard) ───────────────────────
// The per-bar `clampWick` below is ratio-keyed to a bar's OWN close, so it
// is blind to a single-bucket bad print where the spike IS the close
// (open=high=low=close = garbage) — exactly the lone vertical line that
// squashes the whole scale (e.g. one $186K print over a $72K token). A real
// move PERSISTS into the next bar; a bad print REVERTS immediately. So we
// detect an isolated peak — a bar poking far above the surrounding level
// whose neighbours sit back at that level — and pull the whole bar back to
// the surrounding level (effectively deleting the noise). Sustained pumps
// (where the next close stays elevated) are left untouched.
const SPIKE_THRESHOLD = 1.8; // bar high > 1.8× the surrounding level → suspect
const SPIKE_RETURN_BAND = 1.25; // ...and the next close fell back within 1.25× → noise

/**
 * Delete isolated single-bar spikes that the per-bar wick clamp can't see.
 * Operates on display-unit bars AFTER `clampWick`. Endpoints (no neighbour
 * on one side) are skipped — the live forming bar is guarded separately in
 * useLiveCandles.
 * @param bars ascending, de-duped, per-bar-clamped bars
 */
export function clampSpikes(bars: TimedBar[]): TimedBar[] {
  if (bars.length < 3) return bars;
  const out = bars.slice();
  for (let i = 1; i < out.length - 1; i++) {
    const prevC = out[i - 1]!.close;
    const nextC = out[i + 1]!.close;
    if (!(prevC > 0) || !(nextC > 0)) continue;
    // The genuine price level around this bar.
    const base = Math.max(prevC, nextC);
    if (!(base > 0)) continue;
    const cur = out[i]!;
    const pokesUp = cur.high > base * SPIKE_THRESHOLD;
    const pokesDown = cur.low > 0 && cur.low < base / SPIKE_THRESHOLD;
    // Reversion test: the surrounding closes are back at the base level.
    const reverts =
      nextC <= base * SPIKE_RETURN_BAND && prevC <= base * SPIKE_RETURN_BAND;
    if ((pokesUp || pokesDown) && reverts) {
      // Collapse the spike onto the surrounding level: a flat bar at `base`
      // continuous with its neighbours (open = prior close). Volume kept.
      out[i] = {
        ...cur,
        open: prevC,
        high: base,
        low: Math.min(prevC, base),
        close: base,
      };
    }
  }
  return out;
}

/** Clamp the wicks of one OHLC bar. Returns the input untouched when the
 *  close is invalid (≤0) so we never divide by zero. */
export function clampWick(bar: OHLC): OHLC {
  const { close } = bar;
  if (!Number.isFinite(close) || close <= 0) return bar;

  // Stage 1: hard cap at 5×.
  let high = Math.min(bar.high, close * HARD_CAP_MULTIPLE);
  let low = Math.max(bar.low, close / HARD_CAP_MULTIPLE);

  // Stage 2: 200% tolerance band around the close.
  high = Math.min(high, close * (1 + WICK_TOLERANCE));
  low = Math.max(low, close * (1 - WICK_TOLERANCE));

  if (high === bar.high && low === bar.low) return bar;
  return {
    high,
    low,
    close,
    // Keep the open inside the clipped range too (mirrors v1).
    open: Math.min(Math.max(bar.open, low), high),
  };
}

// ── Derived chart colors (C4 crosshair / C7 NOW line) ───────────────
// `useChartColors` (packages/ui) only exposes up/down/accent/ink/line/
// canvas, and it's outside this module's ownership — so the extra chart
// hues are derived here from those base tokens. Colors arrive as either
// "rgb(R G B)" (this repo's CSS-var triples) or any CSS literal.
function parseRgb(color: string): [number, number, number] | null {
  const m = color.match(/rgba?\(\s*(\d+)[\s,]+(\d+)[\s,]+(\d+)/i);
  if (!m) return null;
  return [Number(m[1]), Number(m[2]), Number(m[3])];
}

/** Lighten a color toward white by `amount` (0..1). Used to lift the
 *  crosshair above the canvas (C4). */
export function lighten(base: string, amount: number): string {
  const rgb = parseRgb(base);
  if (!rgb) return base;
  const mix = (c: number) => Math.round(c + (255 - c) * amount);
  return `rgb(${mix(rgb[0])}, ${mix(rgb[1])}, ${mix(rgb[2])})`;
}

// ── C6: subscript-zero price formatting ($0.0₅34) ───────────────────
const SUBSCRIPT_DIGITS = ["₀", "₁", "₂", "₃", "₄", "₅", "₆", "₇", "₈", "₉"];

function toSubscript(n: number): string {
  return String(n)
    .split("")
    .map((d) => SUBSCRIPT_DIGITS[Number(d)] ?? d)
    .join("");
}

/**
 * Format a raw USD price the way Axiom/GMGN do:
 *   - ≥ $1   → 2–4 sig figs with K/M compaction for large numbers
 *   - sub-$1 with ≥4 leading zeros → subscript-zero form, e.g.
 *     0.00000340 → "$0.0₅34" (the ₅ = five zeros after the decimal point)
 *   - otherwise a plain fixed/precision string
 * Used in the axis priceFormatter AND the crosshair tooltip in raw-price
 * mode so the two always agree.
 */
export function formatRawPrice(price: number): string {
  if (!Number.isFinite(price)) return "—";
  if (price === 0) return "$0";
  if (price < 0) return `-${formatRawPrice(-price)}`;

  if (price >= 1_000_000) return `$${(price / 1_000_000).toFixed(2)}M`;
  if (price >= 1_000) return `$${(price / 1_000).toFixed(2)}K`;
  if (price >= 1)
    return `$${price.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: price >= 100 ? 2 : 4 })}`;

  // Sub-dollar: count leading zeros after the decimal point.
  // exp = floor(log10(price)) is negative; #zeros = -exp - 1.
  const exp = Math.floor(Math.log10(price));
  const leadingZeros = -exp - 1;

  if (leadingZeros >= 4) {
    // Significant digits start after the zeros — show ~3, trimmed of any
    // trailing zeros (e.g. 0.00005 → "$0.0₄5", not "$0.0₄500").
    const sig = String(Math.round(price / Math.pow(10, exp - 2))).replace(/0+$/, "") || "0";
    return `$0.0${toSubscript(leadingZeros)}${sig}`;
  }
  // Mid-range sub-dollar: a few sig figs, trailing zeros trimmed.
  const trim = (s: string) => (s.includes(".") ? s.replace(/0+$/, "").replace(/\.$/, "") : s);
  if (price < 0.001) return `$${trim(price.toPrecision(3))}`;
  if (price < 1) return `$${trim(price.toPrecision(4))}`;
  return `$${price.toFixed(2)}`;
}
