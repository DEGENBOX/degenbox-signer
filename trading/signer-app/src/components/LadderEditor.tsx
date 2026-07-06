// Multi-leg TP/SL ladder editor — the app-side port of the web's
// canonical LadderEditor (frontend/modules/trading/src/LadderEditor.tsx).
// Same semantics, same wire shape: leg fractions are anchored to the
// position size at arm time; TP fractions must sum to ≤100%; at most
// one SL (it exits the remainder). Levels are percent vs entry — TP
// fires at Δ ≥ level, SL at Δ ≤ -level. `toLegSpecs` emits the exact
// `LegSpec` JSON the gateway validates.

import { Plus, Trash2, TrendingDown, TrendingUp } from "lucide-react";
import type { LegSpec, PositionTargetRow, TriggerKind } from "../ipc";

/** Editable (string-input) leg row; converted via `toLegSpecs`. */
export interface EditableLeg {
  kind: TriggerKind;
  /** Trigger level in % vs entry (positive for both sides). */
  triggerPct: string;
  /** Sell fraction in % (1..100) — bps on the wire. */
  sellPct: string;
}

// NOTE (v0.3.0 slice 9, spec §E): the premade quick-start templates
// were removed on the operator's call — ladders are fully custom.

/** Mirror of the backend `validate_ladder` — error before round-trip.
 * Returns `null` when valid. */
export function validateEditableLadder(legs: EditableLeg[]): string | null {
  if (legs.length === 0) return "at least one TP or SL leg required";
  if (legs.length > 10) return "too many legs (max 10)";
  let slCount = 0;
  let tpPctSum = 0;
  const tpLevels = new Set<number>();
  for (const leg of legs) {
    const trigger = Number(leg.triggerPct);
    const sell = Number(leg.sellPct);
    if (!Number.isFinite(trigger) || trigger <= 0)
      return leg.kind === "tp" ? "TP level must be > 0" : "SL level must be > 0";
    if (!Number.isFinite(sell) || sell <= 0 || sell > 100)
      return "sell fraction must be 1..100%";
    if (leg.kind === "tp") {
      tpPctSum += sell;
      if (tpLevels.has(trigger)) return "TP legs must have distinct levels";
      tpLevels.add(trigger);
    } else {
      slCount += 1;
    }
  }
  if (slCount > 1) return "at most one SL leg allowed";
  if (tpPctSum > 100) return "TP fractions must sum to ≤ 100%";
  return null;
}

/** Editable rows → wire `LegSpec[]`, TPs ascending then SL. */
export function toLegSpecs(legs: EditableLeg[]): LegSpec[] {
  return [...legs]
    .sort((a, b) =>
      a.kind === b.kind
        ? Number(a.triggerPct) - Number(b.triggerPct)
        : a.kind === "tp"
          ? -1
          : 1,
    )
    .map((l) => ({
      kind: l.kind,
      trigger_pct: String(Number(l.triggerPct)),
      sell_fraction_bps: Math.round(Number(l.sellPct) * 100),
    }));
}

/** Seed editable rows from a stored `LegSpec[]` ladder. */
export function legsFromLadder(ladder: LegSpec[] | null | undefined): EditableLeg[] {
  return (ladder ?? []).map((l) => ({
    kind: l.kind,
    triggerPct: String(Number(l.trigger_pct)),
    sellPct: String(l.sell_fraction_bps / 100),
  }));
}

/** Seed editable rows from an armed ladder's LIVE legs (legacy summary
 * columns as fallback for pre-ladder rows). */
export function legsFromTarget(row: PositionTargetRow): EditableLeg[] {
  const live = (row.legs ?? []).filter(
    (l) => l.status === "active" || l.status === "firing",
  );
  if (live.length > 0) {
    return live.map((l) => ({
      kind: l.kind,
      triggerPct: String(Number(l.trigger_pct)),
      sellPct: String(l.sell_fraction_bps / 100),
    }));
  }
  const out: EditableLeg[] = [];
  const sellPct = String(row.sell_fraction_bps / 100);
  if (row.tp_pct) out.push({ kind: "tp", triggerPct: String(Number(row.tp_pct)), sellPct });
  if (row.sl_pct) out.push({ kind: "sl", triggerPct: String(Number(row.sl_pct)), sellPct });
  return out;
}

/** One-line ladder summary, e.g. "+100% sell 50% · -30% sell 100%". */
export function summarizeLadder(ladder: LegSpec[] | null | undefined): string {
  if (!ladder || ladder.length === 0) return "none";
  return ladder
    .map(
      (l) =>
        `${l.kind === "tp" ? "+" : "-"}${Number(l.trigger_pct)}% sell ${(
          l.sell_fraction_bps / 100
        ).toFixed(0)}%`,
    )
    .join(" · ");
}

export function LadderEditor({
  value,
  onChange,
  disabled,
}: {
  value: EditableLeg[];
  onChange: (legs: EditableLeg[]) => void;
  disabled?: boolean;
}) {
  const hasSl = value.some((l) => l.kind === "sl");
  const tpSum = value
    .filter((l) => l.kind === "tp")
    .reduce((s, l) => s + (Number(l.sellPct) || 0), 0);

  const update = (idx: number, patch: Partial<EditableLeg>) => {
    onChange(value.map((l, i) => (i === idx ? { ...l, ...patch } : l)));
  };
  const remove = (idx: number) => onChange(value.filter((_, i) => i !== idx));
  const addTp = () =>
    onChange([
      ...value.filter((l) => l.kind === "tp"),
      { kind: "tp", triggerPct: "", sellPct: "25" },
      ...value.filter((l) => l.kind === "sl"),
    ]);
  const addSl = () =>
    onChange([...value, { kind: "sl", triggerPct: "30", sellPct: "100" }]);

  return (
    <div style={{ display: "grid", gap: 8 }}>
      {value.length === 0 && (
        <div style={{ fontSize: 12, color: "var(--fg-faint)" }}>
          no legs yet. Add a take-profit or stop-loss level
        </div>
      )}

      {value.map((leg, idx) => {
        const isTp = leg.kind === "tp";
        const Icon = isTp ? TrendingUp : TrendingDown;
        return (
          <div
            key={`${leg.kind}-${idx}`}
            style={{ display: "flex", alignItems: "center", gap: 8 }}
          >
            <span
              className={`badge ${isTp ? "ok" : "fail"}`}
              style={{ width: 44, justifyContent: "center" }}
            >
              <Icon size={10} /> {leg.kind}
            </span>
            <span className="mono" style={{ fontSize: 11, color: "var(--fg-faint)" }}>
              {isTp ? "+" : "-"}
            </span>
            <input
              className="input mono"
              style={{ width: 80, padding: "4px 8px" }}
              inputMode="decimal"
              value={leg.triggerPct}
              placeholder={isTp ? "100" : "30"}
              disabled={disabled}
              aria-label={`${leg.kind} trigger percent`}
              onChange={(e) => update(idx, { triggerPct: e.target.value })}
            />
            <span className="mono" style={{ fontSize: 11, color: "var(--fg-faint)" }}>
              % → sell
            </span>
            <input
              className="input mono"
              style={{ width: 70, padding: "4px 8px" }}
              inputMode="decimal"
              value={leg.sellPct}
              placeholder="50"
              disabled={disabled}
              aria-label={`${leg.kind} sell fraction percent`}
              onChange={(e) => update(idx, { sellPct: e.target.value })}
            />
            <span className="mono" style={{ fontSize: 11, color: "var(--fg-faint)" }}>
              %
            </span>
            <button
              type="button"
              className="btn icon"
              title="Remove leg"
              aria-label="Remove leg"
              disabled={disabled}
              onClick={() => remove(idx)}
            >
              <Trash2 size={12} />
            </button>
          </div>
        );
      })}

      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <button
          type="button"
          className="btn"
          style={{ padding: "3px 9px", fontSize: 11 }}
          disabled={disabled || value.filter((l) => l.kind === "tp").length >= 9}
          onClick={addTp}
        >
          <Plus size={11} /> TP leg
        </button>
        {!hasSl && (
          <button
            type="button"
            className="btn"
            style={{ padding: "3px 9px", fontSize: 11 }}
            disabled={disabled}
            onClick={addSl}
          >
            <Plus size={11} /> SL leg
          </button>
        )}
        <span
          className="mono"
          style={{
            marginLeft: "auto",
            fontSize: 11,
            color: tpSum > 100 ? "var(--red)" : "var(--fg-faint)",
          }}
          title="sum of TP sell fractions (anchored to position size at arm time)"
        >
          TP allocated: {tpSum.toFixed(0)}%
        </span>
      </div>
    </div>
  );
}
