// Per-caller override schema + (de)serialization for the caller-sub
// editor. App-side adaptation of the web's caller override module
// (frontend/modules/alpha-scanner/src/callers/callerOverrides.ts) —
// copied rather than imported because the signer-app workspace does
// not depend on the alpha-scanner module. Field names, units and enum
// variants mirror the Rust `CreateSubReq`
// (crates/modules/execution-computer/src/domain.rs); bps columns are
// surfaced as human percents and converted at the wire boundary here.

import type { CreateSubBody, ExecSubscription } from "./ipc";

// ─── enum variants (mirror computer.rs constants) ──────────────────

/** `sizing_mode` — how the base size is derived. */
export const SIZING_MODE = [
  { value: 0, label: "Fixed USD" },
  { value: 1, label: "% of account" },
] as const;

/** `size_basis` — which account metric the %-of-account path reads. */
export const SIZE_BASIS = [
  { value: 0, label: "Equity" },
  { value: 1, label: "Balance" },
  { value: 2, label: "Available" },
] as const;

/** `size_meaning` — what the resolved size USD represents. */
export const SIZE_MEANING = [
  { value: 0, label: "Position notional" },
  { value: 1, label: "Margin posted" },
  { value: 2, label: "Max risk (SL)" },
] as const;

/** `zone_strategy` — how an entry zone fans into limit orders. */
export const ZONE_STRATEGY = [
  { value: 0, label: "Midpoint (1)" },
  { value: 1, label: "Start + end (2)" },
  { value: 2, label: "Start + mid + end (3)" },
] as const;

/** `market_filter_mode` — whitelist / blacklist gate. */
export const MARKET_FILTER_MODE = [
  { value: 0, label: "Off" },
  { value: 1, label: "Whitelist" },
  { value: 2, label: "Blacklist" },
] as const;

/** `manual_sl_action` — derive a stop when the signal carries none. */
export const MANUAL_SL_ACTION = [
  { value: 0, label: "Off" },
  { value: 1, label: "Fixed %" },
  { value: 2, label: "Trailing %" },
] as const;

/** `margin_mode` — null = inherit caller default. 0 = isolated,
 * 1 = cross (MARGIN_MODE_* in computer.rs). */
export const MARGIN_MODE = [
  { value: 0, label: "Isolated" },
  { value: 1, label: "Cross" },
] as const;

// ─── editable form state ───────────────────────────────────────────

/** String-typed mirror of every editable override. Empty string =
 * unset ("inherit caller default" / keep backend default). */
export interface OverrideState {
  // Sizing
  sizingMode: string; // enum int as string, non-null default "0"
  sizeUsdOverride: string; // USD, nullable
  sizingPctEquity: string; // percent (UI) → bps wire, nullable
  sizeBasis: string; // enum int, non-null default "0"
  sizeMeaning: string; // enum int, non-null default "0"
  sizeLowPercent: string; // integer percent, nullable
  sizeNormalPercent: string; // integer percent, nullable
  sizeHighPercent: string; // integer percent, nullable
  /** UI holds a PERCENT of the entry size (100 = same size); the wire
   * field `dca_size_multiplier` stays a decimal multiplier. */
  dcaSizePct: string;
  skipDca: boolean;
  maxPositionUsd: string; // USD, nullable
  // Leverage
  maxLeverage: string; // hard reject ceiling, nullable
  leverageCap: string; // soft clamp, nullable
  // Risk / SL
  manualSlAction: string; // enum int, non-null default "0"
  manualSlPct: string; // integer percent, nullable
  tpClosePercent: string; // percent (UI) → bps wire (×100)
  zoneStrategy: string; // enum int, non-null default "0"
  slippagePct: string; // percent (UI) → bps wire (×100)
  marginMode: string; // enum int ("" = inherit), nullable
  drawdownStopPct: string; // integer percent, nullable
  // Markets filter
  marketFilterMode: string; // enum int, non-null default "0"
  marketFilterList: string; // comma/space list
}

// NOTE `tier_table_json` ("ramp-in tiers") is deliberately NOT part of
// the editable state (operator feedback R3 2026-07-06 — no v1
// equivalent, unclear semantics). The engine still reads the column;
// the editor simply never sends the field, so any stored table is
// preserved untouched (POST upsert + PATCH both keep omitted fields).

export const EMPTY_OVERRIDES: OverrideState = {
  sizingMode: "0",
  sizeUsdOverride: "",
  sizingPctEquity: "",
  sizeBasis: "0",
  sizeMeaning: "0",
  sizeLowPercent: "",
  sizeNormalPercent: "",
  sizeHighPercent: "",
  dcaSizePct: "",
  skipDca: false,
  maxPositionUsd: "",
  maxLeverage: "",
  leverageCap: "",
  manualSlAction: "0",
  manualSlPct: "",
  tpClosePercent: "",
  zoneStrategy: "0",
  slippagePct: "",
  marginMode: "",
  drawdownStopPct: "",
  marketFilterMode: "0",
  marketFilterList: "",
};

// ─── seed from an existing subscription row ────────────────────────

const intStr = (v: number | null | undefined): string =>
  v === null || v === undefined ? "" : String(v);

/** Seed override state from a subscription row. Non-null enum columns
 * fall back to "0"; nullable columns become "" ("inherit"). */
export function overridesFromSub(sub: ExecSubscription): OverrideState {
  const bpsToPct = (bps: number | null | undefined): string =>
    bps === null || bps === undefined ? "" : trimPct(bps / 100);

  return {
    sizingMode: intStr(sub.sizing_mode ?? 0),
    sizeUsdOverride: sub.size_usd_override ?? "",
    sizingPctEquity: bpsToPct(sub.sizing_pct_equity_bps),
    sizeBasis: intStr(sub.size_basis ?? 0),
    sizeMeaning: intStr(sub.size_meaning ?? 0),
    sizeLowPercent: intStr(sub.size_low_percent),
    sizeNormalPercent: intStr(sub.size_normal_percent),
    sizeHighPercent: intStr(sub.size_high_percent),
    dcaSizePct:
      sub.dca_size_multiplier != null && sub.dca_size_multiplier !== ""
        ? trimPct(Number(sub.dca_size_multiplier) * 100)
        : "",
    skipDca: sub.skip_dca ?? false,
    maxPositionUsd: sub.max_position_usd ?? "",
    maxLeverage: intStr(sub.max_leverage),
    leverageCap: intStr(sub.leverage_cap),
    manualSlAction: intStr(sub.manual_sl_action ?? 0),
    manualSlPct: intStr(sub.manual_sl_pct),
    tpClosePercent: bpsToPct(sub.tp_close_percent_bps),
    zoneStrategy: intStr(sub.zone_strategy ?? 0),
    slippagePct: bpsToPct(sub.slippage_limit_bps),
    marginMode: intStr(sub.margin_mode),
    drawdownStopPct: intStr(sub.drawdown_stop_pct),
    marketFilterMode: intStr(sub.market_filter_mode ?? 0),
    marketFilterList: (sub.market_filter_list ?? []).join(", "),
  };
}

// ─── validation + wire conversion ──────────────────────────────────

export interface OverrideError {
  field: keyof OverrideState;
  message: string;
}

const optInt = (s: string): number | null => {
  const t = s.trim();
  if (t === "") return null;
  const n = Number(t);
  return Number.isFinite(n) ? Math.round(n) : null;
};

const optNum = (s: string): number | null => {
  const t = s.trim();
  if (t === "") return null;
  const n = Number(t);
  return Number.isFinite(n) ? n : null;
};

const optStr = (s: string): string | null => {
  const t = s.trim();
  return t === "" ? null : t;
};

const enumInt = (s: string): number => {
  const n = Number(s);
  return Number.isFinite(n) ? Math.round(n) : 0;
};

/** Build the override slice of a `CreateSubBody` from form state →
 * `[partialBody, errors]`. Blank fields are sent as null (nullable
 * cols) or omitted (defaults); enum selects always send their int.
 * On CREATE (POST upsert) a null falls back to the column default; on
 * EDIT the same body rides PATCH where an explicit null CLEARS the
 * column — so blanking a field genuinely unsets it. Fields the editor
 * no longer shows (ramp-in tier table, legacy market lists) are never
 * sent and therefore never touched. */
export function overridesToBody(
  st: OverrideState,
): [Partial<CreateSubBody>, OverrideError[]] {
  const errors: OverrideError[] = [];
  const body: Partial<CreateSubBody> = {};

  // Sizing
  body.sizing_mode = enumInt(st.sizingMode);
  body.size_usd_override = optStr(st.sizeUsdOverride);
  body.sizing_pct_equity_bps = pctToBps(st.sizingPctEquity);
  body.size_basis = enumInt(st.sizeBasis);
  body.size_meaning = enumInt(st.sizeMeaning);
  body.size_low_percent = optInt(st.sizeLowPercent);
  body.size_normal_percent = optInt(st.sizeNormalPercent);
  body.size_high_percent = optInt(st.sizeHighPercent);
  {
    // Percent in the UI, decimal multiplier on the wire (100% → "1").
    const pct = optNum(st.dcaSizePct);
    if (pct !== null && pct <= 0) {
      errors.push({ field: "dcaSizePct", message: "Add size must be above 0%." });
    }
    body.dca_size_multiplier = pct === null ? null : trimPct(pct / 100);
  }
  body.skip_dca = st.skipDca;
  body.max_position_usd = optStr(st.maxPositionUsd);

  // Leverage
  body.max_leverage = optInt(st.maxLeverage);
  body.leverage_cap = optInt(st.leverageCap);

  // Risk / SL
  body.manual_sl_action = enumInt(st.manualSlAction);
  body.manual_sl_pct = optInt(st.manualSlPct);
  body.tp_close_percent_bps = pctToBps(st.tpClosePercent) ?? undefined;
  body.zone_strategy = enumInt(st.zoneStrategy);
  body.slippage_limit_bps = pctToBps(st.slippagePct) ?? undefined;
  body.margin_mode = st.marginMode === "" ? null : enumInt(st.marginMode);
  body.drawdown_stop_pct = optInt(st.drawdownStopPct);

  // Markets filter
  body.market_filter_mode = enumInt(st.marketFilterMode);
  body.market_filter_list = st.marketFilterList
    .split(/[\s,]+/)
    .map((s) => s.trim().toUpperCase())
    .filter(Boolean);

  // Light cross-field sanity — the backend is the source of truth.
  if (
    enumInt(st.sizingMode) === 1 &&
    body.sizing_pct_equity_bps === null &&
    body.size_low_percent === null &&
    body.size_normal_percent === null &&
    body.size_high_percent === null
  ) {
    errors.push({
      field: "sizingPctEquity",
      message:
        "% of account sizing needs either a % of equity or at least one tier %.",
    });
  }
  if (enumInt(st.manualSlAction) !== 0 && body.manual_sl_pct === null) {
    errors.push({
      field: "manualSlPct",
      message: "Set the stop distance % for the selected SL action.",
    });
  }

  return [body, errors];
}

// percent string → integer bps (×100). 33.33% → 3333. Blank → null.
function pctToBps(s: string): number | null {
  const n = optNum(s);
  if (n === null) return null;
  return Math.round(n * 100);
}

// trim trailing-zero noise from a derived percent for display.
function trimPct(n: number): string {
  return String(Number(n.toFixed(4)));
}
