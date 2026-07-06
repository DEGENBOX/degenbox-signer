// Shared TP/SL ladder editor — ONE widget for every place a sell
// strategy is configured (scanner-preset execution, copy configs,
// per-bot overrides). v0.3.0 slice 9, spec §E / decision D5+D6.
//
// Two wire dialects, one UI:
//
//  * `v2` — the canonical LadderSpec v2 jsonb the platform-ladder crate
//    validates (crates/platform/ladder/src/lib.rs):
//      { version: 2,
//        base_sl?: { from_entry_pct: "-30", sell_bps: 10000 },
//        rungs: [ { gain_pct: "50", sell_bps: 5000,
//                   new_sl?: { from_entry_pct: "10", sell_bps: 10000 } } ] }
//    Sells are % of what's STILL OPEN when the rung fires; each rung
//    can move the stop (stop-loss ladder). Unlimited rungs (≤64).
//    Written to `alpha_presets.bot_config.ladder` — the path bot
//    sessions compile at start (api/bot.rs `preset_ladder`).
//
//  * `legacy` — the older LegSpec[] array some gateway routes still
//    require (`sol_copy_trade_configs.default_ladder`, per-assignment
//    `ladder_override`). No stop moves, sells are % of the position at
//    arm time, TP sells must sum to ≤100%. The widget hides what the
//    wire can't carry — no dead controls.
//
// Client-side validation mirrors platform-ladder `validate()` so bad
// ladders fail before the round-trip.

import { Plus, Trash2 } from "lucide-react";
import type { LegSpec } from "../ipc";

// ─── draft model (string inputs) ────────────────────────────────────

export interface StopDraft {
  /** Signed % from entry ("-30" = 30% below, "10" = 10% above). */
  fromEntryPct: string;
  /** Sell % (1..100). */
  sellPct: string;
}

export interface RungDraft {
  /** Gain % from entry (> 0). */
  gainPct: string;
  /** Sell % of what's still open when this rung fires (1..100). */
  sellPct: string;
  /** Optional stop move once this rung fills (stop-loss ladder). */
  newSl: StopDraft | null;
}

export interface LadderDraft {
  /** The base (first) stop loss — % below entry, sells its own %. */
  baseSl: StopDraft | null;
  rungs: RungDraft[];
}

export const EMPTY_LADDER: LadderDraft = { baseSl: null, rungs: [] };

/** Mirror of platform-ladder MAX_RUNGS. */
export const MAX_RUNGS = 64;

export type LadderDialect = "v2" | "legacy";

// ─── LadderSpec v2 wire shape ───────────────────────────────────────

export interface LadderSpecStop {
  from_entry_pct: string;
  sell_bps: number;
}

export interface LadderSpecRung {
  gain_pct: string;
  sell_bps: number;
  new_sl?: LadderSpecStop;
}

export interface LadderSpecV2 {
  version: 2;
  base_sl?: LadderSpecStop;
  rungs: LadderSpecRung[];
}

// ─── numeric helpers ────────────────────────────────────────────────

function num(v: string): number | null {
  const t = v.trim();
  if (t === "") return null;
  const n = Number(t);
  return Number.isFinite(n) ? n : null;
}

function bps(pctText: string): number {
  return Math.round((num(pctText) ?? 0) * 100);
}

const hasLegs = (d: LadderDraft): boolean => d.baseSl !== null || d.rungs.length > 0;

// ─── validation (mirrors platform_ladder::validate) ─────────────────

/**
 * Validate a draft against the platform-ladder rules. Returns `null`
 * when valid. An EMPTY draft is valid here (meaning "no ladder") —
 * callers that require a ladder check `hasLadderLegs` themselves.
 */
export function validateLadderDraft(
  d: LadderDraft,
  dialect: LadderDialect = "v2",
): string | null {
  if (!hasLegs(d)) return null;
  if (d.rungs.length > MAX_RUNGS) return `too many targets (max ${MAX_RUNGS})`;

  if (d.baseSl) {
    const level = num(d.baseSl.fromEntryPct);
    if (level == null || level >= 0) {
      return "the stop loss must sit below entry (a negative %)";
    }
    const sell = num(d.baseSl.sellPct);
    if (sell == null || sell <= 0 || sell > 100) {
      return "stop-loss sell % must be between 1 and 100";
    }
  }

  let prevGain = -Infinity;
  let tpSellSum = 0;
  for (const [i, r] of d.rungs.entries()) {
    const n = i + 1;
    const gain = num(r.gainPct);
    if (gain == null || gain <= 0) return `target ${n}: gain % must be above 0`;
    if (gain <= prevGain) {
      return `target ${n}: gains must increase from one target to the next`;
    }
    prevGain = gain;
    const sell = num(r.sellPct);
    if (sell == null || sell <= 0 || sell > 100) {
      return `target ${n}: sell % must be between 1 and 100`;
    }
    tpSellSum += sell;
    if (r.newSl) {
      if (dialect === "legacy") {
        return `target ${n}: stop moves aren't supported here yet`;
      }
      const level = num(r.newSl.fromEntryPct);
      if (level == null) return `target ${n}: the new stop needs a level`;
      // Money guard: a stop at/above its own rung's gain would fire the
      // moment the rung fills.
      if (level >= gain) {
        return `target ${n}: the new stop (${level}%) must sit below the target's gain (+${gain}%)`;
      }
      const slSell = num(r.newSl.sellPct);
      if (slSell == null || slSell <= 0 || slSell > 100) {
        return `target ${n}: the new stop's sell % must be between 1 and 100`;
      }
    }
  }

  if (dialect === "legacy") {
    // Legacy sells are anchored to the arm-time position → they must
    // sum to ≤100% (the v2 remaining-basis has no such constraint).
    if (tpSellSum > 100) {
      return "take-profit sells add up to more than 100% of the position";
    }
    const gains = d.rungs.map((r) => num(r.gainPct));
    if (new Set(gains).size !== gains.length) {
      return "each target needs its own gain %";
    }
  }
  return null;
}

// ─── draft ↔ v2 spec ────────────────────────────────────────────────

/** Draft → LadderSpec v2 jsonb. `null` when the draft is empty. */
export function ladderSpecFromDraft(d: LadderDraft): LadderSpecV2 | null {
  if (!hasLegs(d)) return null;
  const spec: LadderSpecV2 = {
    version: 2,
    rungs: d.rungs.map((r) => ({
      gain_pct: String(num(r.gainPct) ?? 0),
      sell_bps: bps(r.sellPct),
      ...(r.newSl
        ? {
            new_sl: {
              from_entry_pct: String(num(r.newSl.fromEntryPct) ?? 0),
              sell_bps: bps(r.newSl.sellPct),
            },
          }
        : {}),
    })),
  };
  if (d.baseSl) {
    spec.base_sl = {
      from_entry_pct: String(num(d.baseSl.fromEntryPct) ?? 0),
      sell_bps: bps(d.baseSl.sellPct),
    };
  }
  return spec;
}

function stopDraft(raw: unknown): StopDraft | null {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const o = raw as Record<string, unknown>;
  const level = Number(o.from_entry_pct);
  const sellBps = Number(o.sell_bps);
  if (!Number.isFinite(level) || !Number.isFinite(sellBps)) return null;
  return { fromEntryPct: String(level), sellPct: String(sellBps / 100) };
}

/** Parse a stored LadderSpec v2 object into a draft. `null` when the
 * value isn't a v2 spec (callers may then try the legacy parser). */
export function draftFromLadderSpec(raw: unknown): LadderDraft | null {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const o = raw as Record<string, unknown>;
  if (!Array.isArray(o.rungs)) return null;
  const rungs: RungDraft[] = [];
  for (const r of o.rungs) {
    if (!r || typeof r !== "object") continue;
    const ro = r as Record<string, unknown>;
    const gain = Number(ro.gain_pct);
    const sellBps = Number(ro.sell_bps);
    if (!Number.isFinite(gain) || !Number.isFinite(sellBps)) continue;
    rungs.push({
      gainPct: String(gain),
      sellPct: String(sellBps / 100),
      newSl: stopDraft(ro.new_sl),
    });
  }
  return { baseSl: stopDraft(o.base_sl), rungs };
}

// ─── draft ↔ legacy LegSpec[] ───────────────────────────────────────

/** Draft → legacy `LegSpec[]` (TPs ascending, then the SL) for routes
 * that still take the old array shape. Stop moves are rejected by
 * `validateLadderDraft(d, "legacy")` before this runs. */
export function legSpecsFromLadderDraft(d: LadderDraft): LegSpec[] {
  const legs: LegSpec[] = [...d.rungs]
    .sort((a, b) => (num(a.gainPct) ?? 0) - (num(b.gainPct) ?? 0))
    .map((r) => ({
      kind: "tp" as const,
      trigger_pct: String(num(r.gainPct) ?? 0),
      sell_fraction_bps: bps(r.sellPct),
    }));
  if (d.baseSl) {
    // Legacy SL triggers are stored positive (% below entry).
    legs.push({
      kind: "sl",
      trigger_pct: String(Math.abs(num(d.baseSl.fromEntryPct) ?? 0)),
      sell_fraction_bps: bps(d.baseSl.sellPct),
    });
  }
  return legs;
}

/** Legacy `LegSpec[]` → draft (SL trigger flips to a signed level). */
export function ladderDraftFromLegSpecs(
  ladder: LegSpec[] | null | undefined,
): LadderDraft {
  const rungs: RungDraft[] = [];
  let baseSl: StopDraft | null = null;
  for (const l of ladder ?? []) {
    if (l.kind === "tp") {
      rungs.push({
        gainPct: String(Number(l.trigger_pct)),
        sellPct: String(l.sell_fraction_bps / 100),
        newSl: null,
      });
    } else {
      baseSl = {
        fromEntryPct: String(-Math.abs(Number(l.trigger_pct))),
        sellPct: String(l.sell_fraction_bps / 100),
      };
    }
  }
  rungs.sort((a, b) => (num(a.gainPct) ?? 0) - (num(b.gainPct) ?? 0));
  return { baseSl, rungs };
}

/** Legacy bot_config `take_profits` [{mult, pct_to_sell}] +
 * `stop_loss_pct` → draft. Seeds the editor for presets written before
 * the v2 `ladder` key existed (those keys never reached execution, so
 * this is a convenience import, not a semantic migration). */
export function ladderDraftFromLegacyBotConfig(
  takeProfits: Array<{ mult: number; pct_to_sell: number }>,
  stopLossPct: number | null,
): LadderDraft {
  return {
    baseSl:
      stopLossPct != null
        ? { fromEntryPct: String(-Math.abs(stopLossPct)), sellPct: "100" }
        : null,
    rungs: takeProfits
      .filter((l) => l.mult > 1)
      .map((l) => ({
        gainPct: String(Number(((l.mult - 1) * 100).toPrecision(10))),
        sellPct: String(l.pct_to_sell),
        newSl: null,
      })),
  };
}

// ─── summaries (list chips) ─────────────────────────────────────────

/** One-line summary for cards/tables, e.g.
 * "SL -30% · +50% sell 50% → stop +10% · +150% sell 50%". */
export function summarizeLadderDraft(d: LadderDraft): string {
  if (!hasLegs(d)) return "";
  const parts: string[] = [];
  if (d.baseSl) parts.push(`SL ${num(d.baseSl.fromEntryPct) ?? 0}%`);
  for (const r of d.rungs) {
    let p = `+${num(r.gainPct) ?? 0}% sell ${num(r.sellPct) ?? 0}%`;
    if (r.newSl) {
      const lvl = num(r.newSl.fromEntryPct) ?? 0;
      p += ` → stop ${lvl >= 0 ? "+" : ""}${lvl}%`;
    }
    parts.push(p);
  }
  return parts.join(" · ");
}

/** Summary straight from a stored spec value (v2 object or legacy
 * array), for read-only cards. Empty string when nothing is set. */
export function summarizeStoredLadder(raw: unknown): string {
  const v2 = draftFromLadderSpec(raw);
  if (v2) return summarizeLadderDraft(v2);
  if (Array.isArray(raw)) {
    return summarizeLadderDraft(ladderDraftFromLegSpecs(raw as LegSpec[]));
  }
  return "";
}

/** Plain-words, one-line description of what the ladder will do, e.g.
 * "2 targets: sell 50% at +50%, then 50% of what's left at +150%;
 *  after target 1 the stop moves to +10%; stop loss sells 100% at -30%."
 * `null` when the draft is empty. */
export function describeLadderDraft(
  d: LadderDraft,
  dialect: LadderDialect = "v2",
): string | null {
  if (!hasLegs(d)) return null;
  const bits: string[] = [];
  if (d.rungs.length > 0) {
    const targets = d.rungs.map((r, i) => {
      const gain = num(r.gainPct) ?? 0;
      const sell = num(r.sellPct) ?? 0;
      const share =
        dialect === "v2" && i > 0 ? `${sell}% of what's left` : `${sell}%`;
      return `${share} at +${gain}%`;
    });
    const n = d.rungs.length;
    bits.push(`${n} ${n === 1 ? "target" : "targets"}: sell ${targets.join(", then ")}`);
    d.rungs.forEach((r, i) => {
      if (!r.newSl) return;
      const lvl = num(r.newSl.fromEntryPct) ?? 0;
      bits.push(
        `after target ${i + 1} the stop moves to ${lvl >= 0 ? "+" : ""}${lvl}%`,
      );
    });
  }
  if (d.baseSl) {
    const lvl = num(d.baseSl.fromEntryPct) ?? 0;
    const sell = num(d.baseSl.sellPct) ?? 0;
    bits.push(`stop loss sells ${sell}% at ${lvl}%`);
  }
  const line = bits.join("; ");
  return line.charAt(0).toUpperCase() + line.slice(1) + ".";
}

// ─── the editor ─────────────────────────────────────────────────────

interface Props {
  value: LadderDraft;
  onChange: (next: LadderDraft) => void;
  disabled?: boolean;
  /** `legacy` hides the per-target stop moves the old wire can't carry
   * and labels sells as % of the position (anchored semantics). */
  dialect?: LadderDialect;
}

export function LadderSpecEditor({
  value,
  onChange,
  disabled,
  dialect = "v2",
}: Props) {
  const v2 = dialect === "v2";

  const setRung = (idx: number, patch: Partial<RungDraft>) =>
    onChange({
      ...value,
      rungs: value.rungs.map((r, i) => (i === idx ? { ...r, ...patch } : r)),
    });

  const addRung = () => {
    const last = value.rungs[value.rungs.length - 1];
    const lastGain = last ? num(last.gainPct) : null;
    onChange({
      ...value,
      rungs: [
        ...value.rungs,
        {
          gainPct: lastGain != null ? String(lastGain * 2) : "100",
          sellPct: "50",
          newSl: null,
        },
      ],
    });
  };

  const removeRung = (idx: number) =>
    onChange({ ...value, rungs: value.rungs.filter((_, i) => i !== idx) });

  const summary = describeLadderDraft(value, dialect);

  return (
    <div className="ladder-spec">
      {/* column header — only when there is something to align */}
      {hasLegs(value) && (
        <div className="ladder-cols ladder-head" aria-hidden>
          <span />
          <span className="ladder-col-label">Gain from entry</span>
          <span className="ladder-col-label">
            {v2 ? "Sell (of what's left)" : "Sell (of the position)"}
          </span>
          <span />
        </div>
      )}

      {/* take-profit rungs */}
      {value.rungs.map((r, idx) => (
        <div key={idx} className="ladder-rung">
          <div className="ladder-cols">
            <span className="badge ok ladder-tag">TP{idx + 1}</span>
            <span className="ladder-cell">
              <span className="ladder-word">+</span>
              <input
                className="input mono ladder-num"
                inputMode="decimal"
                value={r.gainPct}
                placeholder="100"
                disabled={disabled}
                aria-label={`Target ${idx + 1} gain percent from entry`}
                onChange={(e) => setRung(idx, { gainPct: e.target.value })}
              />
              <span className="ladder-word">%</span>
            </span>
            <span
              className="ladder-cell"
              title={
                v2
                  ? "Percent of whatever is still open when this target hits. Selling 50% three times leaves an eighth running."
                  : "Percent of the position as it was when the ladder was armed."
              }
            >
              <input
                className="input mono ladder-num"
                inputMode="decimal"
                value={r.sellPct}
                placeholder="50"
                disabled={disabled}
                aria-label={`Target ${idx + 1} sell percent`}
                onChange={(e) => setRung(idx, { sellPct: e.target.value })}
              />
              <span className="ladder-word">%</span>
            </span>
            <button
              type="button"
              className="btn icon"
              title="Remove this target"
              aria-label={`Remove target ${idx + 1}`}
              disabled={disabled}
              onClick={() => removeRung(idx)}
            >
              <Trash2 size={12} />
            </button>
          </div>

          {v2 &&
            (r.newSl ? (
              <div className="ladder-row ladder-newsl">
                <span className="ladder-word indent">then move the stop to</span>
                <input
                  className="input mono ladder-num"
                  inputMode="decimal"
                  value={r.newSl.fromEntryPct}
                  placeholder="10"
                  disabled={disabled}
                  aria-label={`Target ${idx + 1} new stop level, % from entry`}
                  onChange={(e) =>
                    setRung(idx, { newSl: { ...r.newSl!, fromEntryPct: e.target.value } })
                  }
                />
                <span
                  className="ladder-word"
                  title="Signed % from entry. Positive locks in profit (e.g. +10% after the first target); negative keeps a loss stop."
                >
                  % and sell
                </span>
                <input
                  className="input mono ladder-num sm"
                  inputMode="decimal"
                  value={r.newSl.sellPct}
                  placeholder="100"
                  disabled={disabled}
                  aria-label={`Target ${idx + 1} new stop sell percent`}
                  onChange={(e) =>
                    setRung(idx, { newSl: { ...r.newSl!, sellPct: e.target.value } })
                  }
                />
                <span className="ladder-word">% there</span>
                <button
                  type="button"
                  className="btn icon"
                  title="Remove the stop move"
                  aria-label={`Remove target ${idx + 1} stop move`}
                  disabled={disabled}
                  onClick={() => setRung(idx, { newSl: null })}
                >
                  <Trash2 size={12} />
                </button>
              </div>
            ) : (
              <div className="ladder-row ladder-newsl">
                <button
                  type="button"
                  className="btn xs indent"
                  disabled={disabled}
                  title="After this target fills, replace the live stop with a new one. Set it above entry to lock in profit."
                  onClick={() =>
                    setRung(idx, { newSl: { fromEntryPct: "0", sellPct: "100" } })
                  }
                >
                  <Plus size={11} /> Move the stop after this target
                </button>
              </div>
            ))}
        </div>
      ))}

      {/* base stop loss */}
      {value.baseSl ? (
        <div className="ladder-cols">
          <span className="badge fail ladder-tag">SL</span>
          <span
            className="ladder-cell"
            title="The base stop, live from entry. Negative = below entry."
          >
            <input
              className="input mono ladder-num"
              inputMode="decimal"
              value={value.baseSl.fromEntryPct}
              placeholder="-30"
              disabled={disabled}
              aria-label="Stop-loss level, % from entry (negative)"
              onChange={(e) =>
                onChange({ ...value, baseSl: { ...value.baseSl!, fromEntryPct: e.target.value } })
              }
            />
            <span className="ladder-word">%</span>
          </span>
          <span className="ladder-cell">
            <input
              className="input mono ladder-num"
              inputMode="decimal"
              value={value.baseSl.sellPct}
              placeholder="100"
              disabled={disabled}
              aria-label="Stop-loss sell percent"
              onChange={(e) =>
                onChange({ ...value, baseSl: { ...value.baseSl!, sellPct: e.target.value } })
              }
            />
            <span className="ladder-word">%</span>
          </span>
          <button
            type="button"
            className="btn icon"
            title="Remove the stop loss"
            aria-label="Remove the stop loss"
            disabled={disabled}
            onClick={() => onChange({ ...value, baseSl: null })}
          >
            <Trash2 size={12} />
          </button>
        </div>
      ) : null}

      {!hasLegs(value) && (
        <div className="ladder-empty">
          No targets yet. Add a take-profit level, a stop loss, or both.
        </div>
      )}

      <div className="ladder-row ladder-actions">
        <button
          type="button"
          className="btn"
          disabled={disabled || value.rungs.length >= MAX_RUNGS}
          onClick={addRung}
        >
          <Plus size={11} /> Add take-profit target
        </button>
        {!value.baseSl && (
          <button
            type="button"
            className="btn"
            disabled={disabled}
            onClick={() =>
              onChange({ ...value, baseSl: { fromEntryPct: "-30", sellPct: "100" } })
            }
          >
            <Plus size={11} /> Add stop loss
          </button>
        )}
      </div>

      {summary && <p className="ladder-summary">{summary}</p>}
    </div>
  );
}
