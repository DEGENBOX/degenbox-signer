// Data plumbing for the Perpetuals Positions tab (W4.1) — realized-PnL
// windows, the gateway's enriched open-positions view (mark / leverage /
// liquidation / source attribution) and HL candle history. Everything
// rides the generic `gateway_fetch` proxy (src/lib/gateway.ts): the
// desktop JWT stays Rust-side, no bespoke Rust DTO per endpoint.
//
// NOTE on numbers: the gateway serialises Decimals as JSON STRINGS
// (rust_decimal serde default) — parse per field with Number().

import { gwGet } from "../../lib/gateway";

// Shared lenient parsing/formatting helpers — imported (read-only) from
// the Sol positions feature so the two tabs render numbers identically.
export { num, compactNum } from "../positions/data";
import { num } from "../positions/data";

// ─── GET /api/hyperliquid/pnl/windows (W2.2 contract) ──────────────
//
// Envelope is contract-locked with the Solana twin
// (`/api/trading/pnl/windows`): clients[] + totals, USD buckets as
// decimal strings. HL has no lamport leg — `realized_usd` only.

export interface UsdWindows {
  d1: string;
  d7: string;
  d30: string;
}

export interface HlPnlWindowClient {
  /** `trading_clients.id` when the wallet maps to a live HL client;
   *  null for legacy/unregistered wallets. */
  client_id: string | null;
  /** Lowercase 0x… HL master wallet. */
  wallet: string;
  chain: string;
  realized_usd: UsdWindows;
}

export interface HlPnlWindowsResponse {
  clients: HlPnlWindowClient[];
  totals: { realized_usd: UsdWindows };
}

export const fetchPnlWindows = () =>
  gwGet<HlPnlWindowsResponse>("/api/hyperliquid/pnl/windows");

// ─── GET /api/hyperliquid/wallets/{addr}/positions ─────────────────
//
// Live clearinghouseState view (30 s gateway cache, subscriber-gated):
// the fields the local daemon snapshot lacks — mark, leverage,
// liquidation price, margin used, funding and source attribution
// (`manual` | `caller:{id}` | `wallet:{address}`).

export interface GwPerpPosition {
  coin: string;
  /** Signed size — positive = long, negative = short. */
  szi: string | number;
  entry_px: string | null;
  mark_px: string | null;
  liquidation_px: string | null;
  unrealized_pnl: string | null;
  margin_used: string | null;
  funding_since_open?: string | null;
  leverage: number | null;
  source?: string | null;
}

export const fetchGwPositions = (address: string) =>
  gwGet<GwPerpPosition[]>(
    `/api/hyperliquid/wallets/${encodeURIComponent(address.toLowerCase())}/positions`,
  );

/** `manual` / `caller:{id}` / `wallet:{address}` → short display label. */
export function sourceLabel(source: string | null | undefined): string {
  if (!source) return "—";
  if (source === "manual") return "manual";
  if (source.startsWith("caller:")) {
    const id = source.slice("caller:".length);
    return `caller ${id.length > 8 ? `${id.slice(0, 8)}…` : id}`;
  }
  if (source.startsWith("wallet:")) {
    const a = source.slice("wallet:".length);
    return `copy ${a.length > 10 ? `${a.slice(0, 6)}…${a.slice(-4)}` : a}`;
  }
  return source;
}

// ─── Candle history (gateway HL proxy, 10 s cache) ─────────────────
//
// GET /api/hyperliquid/candles/{coin}?interval=&start=&end= — same
// route the web's per-market page uses; the gateway whitelists the
// interval and forwards to HL's public `candleSnapshot`. Bars arrive
// in HL wire shape; we normalise to the Sol chart's `Candle` so the
// ported chart code stays line-identical.

export type PerpInterval = "1m" | "5m" | "15m" | "1h" | "4h" | "1d";

export const INTERVAL_SECS: Record<PerpInterval, number> = {
  "1m": 60,
  "5m": 300,
  "15m": 900,
  "1h": 3_600,
  "4h": 14_400,
  "1d": 86_400,
};

/** HL `candleSnapshot` bar — numerics arrive as strings. */
interface HlWireCandle {
  /** Bar start, ms since epoch. */
  t: number;
  o: string;
  c: string;
  h: string;
  l: string;
  /** Coin-denominated volume. */
  v: string;
}

/** Normalised bar — field-compatible with the Sol chart's `Candle`
 *  (features/positions/data) so the ported chart renders both. */
export interface Candle {
  ts: string;
  open_usd: string;
  high_usd: string;
  low_usd: string;
  close_usd: string;
  volume_usd: string;
}

export async function fetchPerpCandles(
  coin: string,
  interval: PerpInterval,
  startMs: number,
  endMs: number,
): Promise<Candle[]> {
  const rows = await gwGet<HlWireCandle[]>(
    `/api/hyperliquid/candles/${encodeURIComponent(coin)}?interval=${interval}&start=${Math.floor(
      startMs,
    )}&end=${Math.floor(endMs)}`,
  );
  return rows.map((k) => {
    // HL volume is coin-denominated — approximate USD with the bar close
    // so the histogram reads in the same unit as the axis.
    const volUsd = (Number(k.v) || 0) * (Number(k.c) || 0);
    return {
      ts: new Date(k.t).toISOString(),
      open_usd: k.o,
      high_usd: k.h,
      low_usd: k.l,
      close_usd: k.c,
      volume_usd: String(volUsd),
    };
  });
}

// ─── Perp price formatting ──────────────────────────────────────────
//
// `formatRawPrice` (chartFormat) compacts ≥$1K to "$97.04K" — right for
// memecoin MCaps, wrong for a BTC mark. Perps keep full figures above
// $1 and fall back to the subscript-zero form only for sub-dollar alts.

import { formatRawPrice } from "../positions/chart/chartFormat";

export function formatPerpPrice(v: number): string {
  if (!Number.isFinite(v)) return "—";
  if (v < 0) return `-${formatPerpPrice(-v)}`;
  if (v >= 1_000) {
    return `$${v.toLocaleString("en-US", { maximumFractionDigits: 1 })}`;
  }
  if (v >= 1) {
    return `$${v.toLocaleString("en-US", {
      minimumFractionDigits: 2,
      maximumFractionDigits: 4,
    })}`;
  }
  return formatRawPrice(v);
}

/** Derive the mark when the gateway row is missing/stale:
 *  uPnL = szi × (mark − entry) → mark = entry + uPnL / szi (signed szi
 *  handles both directions). */
export function deriveMark(
  entryPx: string | number | null | undefined,
  upnl: string | number | null | undefined,
  szi: string | number | null | undefined,
): number | null {
  const entry = num(entryPx as string | number | null);
  const pnl = num(upnl as string | number | null);
  const size = num(szi as string | number | null);
  if (entry == null || pnl == null || size == null || size === 0) return null;
  const mark = entry + pnl / size;
  return Number.isFinite(mark) && mark > 0 ? mark : null;
}
