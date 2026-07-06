// Data plumbing for the Positions tab (W3.1) — PnL windows, intent
// attribution, candle history. Everything rides the generic
// `gateway_fetch` proxy (src/lib/gateway.ts): the desktop JWT stays
// Rust-side, no bespoke Rust DTO per endpoint.
//
// NOTE on numbers: the gateway serialises Decimals as JSON STRINGS
// (rust_decimal serde default) — parse per field with Number().

import { gwGet, gwPost } from "../../lib/gateway";

export const LAMPORTS_PER_SOL = 1_000_000_000;

// ─── GET /api/trading/pnl/windows (W2.1 contract, FIXED) ──────────

export interface LamportWindows {
  d1: number;
  d7: number;
  d30: number;
}

/** USD sums arrive as decimal strings; `null` object = no event in
 *  the 30d window carried a USD snapshot. */
export interface UsdWindows {
  d1: string;
  d7: string;
  d30: string;
}

export interface ClientPnlWindows {
  client_id: string;
  wallet: string;
  chain: string;
  realized_lamports: LamportWindows;
  realized_usd: UsdWindows | null;
}

export interface PnlWindowsResponse {
  clients: ClientPnlWindows[];
  totals: {
    realized_lamports: LamportWindows;
    realized_usd: UsdWindows | null;
  };
}

export const fetchPnlWindows = () =>
  gwGet<PnlWindowsResponse>("/api/trading/pnl/windows");

// ─── Position → client/preset/copy attribution ────────────────────
//
// `trading_positions` rows are per (owner, mint) — they carry no
// client/preset linkage. The INTENTS do (`client_id`, `preset_id`,
// `copy_config_id`), so we derive a best-effort source map from the
// user's most recent 100 intents (the gateway's fixed page size):
// newest buy intent per output mint wins. Positions whose buys have
// scrolled out of that window degrade to "manual / unattributed".

interface IntentLite {
  side: string;
  status: string;
  output_mint: string;
  preset_id: string | null;
  copy_config_id: string | null;
  client_id: string | null;
}

export interface PositionSource {
  clientId: string | null;
  presetId: string | null;
  copyConfigId: string | null;
}

export async function fetchSourceMap(): Promise<Map<string, PositionSource>> {
  const intents = await gwGet<IntentLite[]>("/api/trading/intents");
  const map = new Map<string, PositionSource>();
  // Newest-first from the gateway — first hit per mint wins.
  for (const i of intents) {
    if (i.side !== "buy") continue;
    if (i.status === "failed" || i.status === "cancelled" || i.status === "expired") continue;
    if (map.has(i.output_mint)) continue;
    map.set(i.output_mint, {
      clientId: i.client_id,
      presetId: i.preset_id,
      copyConfigId: i.copy_config_id,
    });
  }
  return map;
}

// ─── Candle history (alpha-scanner HTTP endpoint) ─────────────────
//
// Same endpoint the web ChartPanel uses:
//   GET /api/alpha/tokens/{chain}/{addr}/history?interval_secs=&limit=[&before=]
// Subscriber-gated (403 for non-subscribers). Solana chain_id = 1.

export const SOL_CHAIN_ID = 1;

export interface Candle {
  ts: string;
  open_usd: string;
  high_usd: string;
  low_usd: string;
  close_usd: string;
  volume_usd: string;
}

export function fetchCandles(
  address: string,
  intervalSecs: number,
  limit: number,
  beforeIso?: string,
): Promise<Candle[]> {
  const before = beforeIso ? `&before=${encodeURIComponent(beforeIso)}` : "";
  return gwGet<Candle[]>(
    `/api/alpha/tokens/${SOL_CHAIN_ID}/${address}/history?interval_secs=${intervalSecs}&limit=${limit}${before}`,
  );
}

/** Fire-and-forget GeckoTerminal backfill so old positions whose token
 *  left the live feed still chart back to launch. Errors swallowed —
 *  non-subscriber / already fresh / transient all degrade to "render
 *  what exists". */
export function requestBackfill(address: string): Promise<void> {
  return gwPost(`/api/alpha/tokens/${SOL_CHAIN_ID}/${address}/backfill`, {}).then(
    () => undefined,
    () => undefined,
  );
}

// ─── small shared helpers ──────────────────────────────────────────

export const lamportsToSol = (l: number) => l / LAMPORTS_PER_SOL;

/** Lenient Decimal-string/number → finite number (else null). */
export function num(v: string | number | null | undefined): number | null {
  if (v === null || v === undefined) return null;
  const n = typeof v === "string" ? Number(v) : v;
  return Number.isFinite(n) ? n : null;
}

/** "12.3K" / "4.56M" compact for MCAPs — bare (no $; unit handled by
 *  the column header / toggle). */
export function compactNum(v: number | null): string {
  if (v == null || !Number.isFinite(v)) return "—";
  const abs = Math.abs(v);
  const sign = v < 0 ? "-" : "";
  if (abs >= 1e9) return `${sign}${(abs / 1e9).toFixed(2)}B`;
  if (abs >= 1e6) return `${sign}${(abs / 1e6).toFixed(2)}M`;
  if (abs >= 1e3) return `${sign}${(abs / 1e3).toFixed(1)}K`;
  if (abs >= 1) return `${sign}${abs.toFixed(2)}`;
  return `${sign}${abs.toPrecision(3)}`;
}

/** SOL amount display — tiered decimals, no unit suffix. */
export function fmtSolAmt(v: number | null): string {
  if (v == null || !Number.isFinite(v)) return "—";
  const abs = Math.abs(v);
  if (abs >= 100) return v.toFixed(1);
  if (abs >= 1) return v.toFixed(2);
  if (abs >= 0.01) return v.toFixed(3);
  return v.toFixed(4);
}
