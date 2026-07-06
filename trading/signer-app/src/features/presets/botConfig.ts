// Preset bot_config — parse / merge helpers for the execution + sell
// strategy a scanner preset ships to the local signer.
//
// Canonical trading keys inside `alpha_presets.bot_config`:
//   buy_size_lamports, slippage_bps, tip_lamports, max_concurrent,
//   ladder  ← LadderSpec v2 jsonb; THIS is what execution reads —
//             bot sessions started from the preset compile it
//             (crates/modules/trading/src/api/bot.rs `preset_ladder`).
//
// Legacy keys `take_profits` [{mult, pct_to_sell}] + `stop_loss_pct`
// were written by older app builds but never reached execution (audit
// finding #5). They're still PARSED (to seed the editor once) and are
// stripped on every save — `ladder` replaces them.
//
// The SAME JSONB also carries non-trading keys the scanner owns
// (`notifications_enabled`, `count_sources`, …) and
// `PATCH /api/alpha/presets/{id}` REPLACES the whole blob — so every
// write goes through `mergeBotConfig`, which preserves any key it does
// not explicitly manage.

import type { LadderSpecV2 } from "../../components/LadderSpecEditor";

/** One LEGACY take-profit leg (read-only; never written any more). */
export interface TakeProfitLeg {
  mult: number;
  pct_to_sell: number;
}

/** Parsed trading view of bot_config. `null` field = unset. */
export interface PresetBotConfig {
  buySizeLamports: number | null;
  slippageBps: number | null;
  tipLamports: number | null;
  maxConcurrent: number | null;
  /** The TP/SL ladder (LadderSpec v2) — raw jsonb, editor-validated. */
  ladder: unknown | null;
  /** LEGACY read-only seed — see module header. */
  takeProfits: TakeProfitLeg[];
  /** LEGACY read-only seed — see module header. */
  stopLossPct: number | null;
}

/** The keys this surface owns inside the blob (all stripped before a
 * write; the legacy pair is write-never, strip-always). */
const TRADING_KEYS = [
  "buy_size_lamports",
  "slippage_bps",
  "tip_lamports",
  "max_concurrent",
  "ladder",
  "take_profits",
  "stop_loss_pct",
] as const;

function num(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  // Decimals can round-trip as strings depending on the writer.
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

function asObject(raw: unknown): Record<string, unknown> {
  return raw && typeof raw === "object" && !Array.isArray(raw)
    ? (raw as Record<string, unknown>)
    : {};
}

export function parseBotConfig(raw: unknown): PresetBotConfig {
  const obj = asObject(raw);
  const tps: TakeProfitLeg[] = [];
  if (Array.isArray(obj.take_profits)) {
    for (const leg of obj.take_profits) {
      const l = asObject(leg);
      const mult = num(l.mult);
      const pct = num(l.pct_to_sell);
      if (mult != null && pct != null) tps.push({ mult, pct_to_sell: pct });
    }
  }
  return {
    buySizeLamports: num(obj.buy_size_lamports),
    slippageBps: num(obj.slippage_bps),
    tipLamports: num(obj.tip_lamports),
    maxConcurrent: num(obj.max_concurrent),
    ladder: obj.ladder ?? null,
    takeProfits: tps,
    stopLossPct: num(obj.stop_loss_pct),
  };
}

/** Any trading key set ⇒ the preset is an autotrade preset (vs
 *  filter-only). */
export function hasExecutionConfig(cfg: PresetBotConfig): boolean {
  return (
    cfg.buySizeLamports != null ||
    cfg.slippageBps != null ||
    cfg.tipLamports != null ||
    cfg.maxConcurrent != null ||
    cfg.ladder != null ||
    cfg.takeProfits.length > 0 ||
    cfg.stopLossPct != null
  );
}

/**
 * Merge edited trading fields into the EXISTING bot_config blob without
 * clobbering foreign keys. `null` fields remove their key; the legacy
 * `take_profits`/`stop_loss_pct` pair is always stripped (superseded by
 * `ladder`). Returns `null` when the merge leaves an empty object (so
 * a filter-only preset stores SQL NULL, not `{}`).
 */
export function mergeBotConfig(
  raw: unknown,
  cfg: {
    buySizeLamports: number | null;
    slippageBps: number | null;
    tipLamports: number | null;
    maxConcurrent: number | null;
    ladder: LadderSpecV2 | null;
  },
): Record<string, unknown> | null {
  const out: Record<string, unknown> = { ...asObject(raw) };
  for (const k of TRADING_KEYS) delete out[k];
  if (cfg.buySizeLamports != null) out.buy_size_lamports = Math.round(cfg.buySizeLamports);
  if (cfg.slippageBps != null) out.slippage_bps = Math.round(cfg.slippageBps);
  if (cfg.tipLamports != null) out.tip_lamports = Math.round(cfg.tipLamports);
  if (cfg.maxConcurrent != null) out.max_concurrent = Math.round(cfg.maxConcurrent);
  if (cfg.ladder != null) out.ladder = cfg.ladder;
  return Object.keys(out).length > 0 ? out : null;
}

/** Compact one-line execution summary for the preset card, e.g.
 *  "0.25 SOL · 2% slip · tip 0.001 · ≤3 open". Empty string when
 *  filter-only. The ladder renders separately. */
export function summarizeExecution(cfg: PresetBotConfig): string {
  const parts: string[] = [];
  if (cfg.buySizeLamports != null) {
    parts.push(`${trim(cfg.buySizeLamports / 1e9)} SOL`);
  }
  if (cfg.slippageBps != null) parts.push(`${trim(cfg.slippageBps / 100)}% slip`);
  if (cfg.tipLamports != null) parts.push(`tip ${trim(cfg.tipLamports / 1e9)}`);
  if (cfg.maxConcurrent != null) parts.push(`≤${cfg.maxConcurrent} open`);
  return parts.join(" · ");
}

function trim(n: number): string {
  return String(Number(n.toPrecision(12)));
}
