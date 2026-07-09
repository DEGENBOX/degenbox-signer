// Per-caller override schema + (de)serialization for the caller-sub
// editor. App-side adaptation of the web's caller override module
// (frontend/modules/alpha-scanner/src/callers/callerOverrides.ts) —
// copied rather than imported because the signer-app workspace does
// not depend on the alpha-scanner module. Field names, units and enum
// variants mirror the Rust `CreateSubReq`
// (crates/modules/execution-computer/src/domain.rs); bps columns are
// surfaced as human percents and converted at the wire boundary here.
//
// v1-faithful sizing (operator feedback 2026-07-09, parity with the web
// dashboard's corrected model): `sizing_mode` is an EXPLICIT two-way
// toggle — "% of account" (mode 1) vs "$ per trade" (mode 0) — NOT
// derived from a filled field. Both modes carry the SAME three
// conviction tiers Small / Normal / High: % mode writes the
// `size_*_percent` columns, $ mode writes the `size_*_usd` columns
// (migration 20260711150000). A save sends only the ACTIVE mode's three
// tiers, plus the always-on `size_basis` (%-mode account metric) and
// `size_meaning`. The legacy `size_usd_override` is NEVER written (only
// read to seed a legacy sub's Normal-$ tier).
//
// The power fields that duplicated or muddied the model (size_multiplier,
// max_size_usd, sizing_pct_equity, leverage_cap, margin_mode,
// tp_close_percent, tier_table_json) are gone from BOTH the UI and the
// save payload — their keys are simply never sent, so on an existing sub
// PATCH omits them and the stored column is left untouched.

import type { CreateSubBody, ExecSubscription } from "./ipc";

// ─── enum variants (mirror computer.rs constants) ──────────────────

/** `sizing_mode` — the explicit two-way size toggle. 1 = %-of-account
 *  conviction tiers, 0 = fixed $ per-trade conviction tiers. */
export const SIZING_MODE = [
  { value: 1, label: "% of account" },
  { value: 0, label: "$ per trade" },
] as const;

/** `size_basis` — which account metric the %-of-account tiers measure
 *  against. Only meaningful in % mode. */
export const SIZE_BASIS = [
  { value: 0, label: "Equity" },
  { value: 1, label: "Balance" },
  { value: 2, label: "Available" },
] as const;

/** `size_meaning` — what the size number represents. */
export const SIZE_MEANING = [
  { value: 0, label: "Position size" },
  { value: 1, label: "Margin" },
  { value: 2, label: "SL risk" },
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

/** `manual_sl_action` — derive a stop when the signal carries none.
 *  Trailing (2) is still honoured by the engine but is no longer an
 *  option in this editor; only No stop / Fixed % are offered. */
export const MANUAL_SL_ACTION = [
  { value: 0, label: "No stop" },
  { value: 1, label: "Fixed %" },
] as const;

// ─── editable form state ───────────────────────────────────────────

/** String-typed mirror of every editable field. Empty string = unset
 *  ("inherit caller default" / keep backend default). */
export interface OverrideState {
  /** Following — the sub executes new calls while on. */
  enabled: boolean;
  // Size — explicit mode toggle: "1" = %-of-account, "0" = $ per trade.
  sizingMode: string; // enum int as string, non-null default "1"
  // Conviction tiers (%-of-account) — active in % mode.
  sizeLowPercent: string; // integer percent, nullable
  sizeNormalPercent: string; // integer percent, nullable
  sizeHighPercent: string; // integer percent, nullable
  // Conviction tiers ($ per trade) — active in $ mode.
  sizeLowUsd: string; // USD, nullable
  sizeNormalUsd: string; // USD, nullable
  sizeHighUsd: string; // USD, nullable
  // Account metric the % tiers measure against (% mode only).
  sizeBasis: string; // enum int as string, non-null default "0"
  // What the size number means (both modes).
  sizeMeaning: string; // enum int as string, non-null default "0"
  // Leverage
  leverageOverride: string; // replaces the call's leverage, nullable
  maxLeverage: string; // hard reject ceiling, nullable
  // Max position
  maxPositionUsd: string; // USD, nullable — CLAMPS oversized orders down
  // Stop-loss fallback
  manualSlAction: string; // enum int, non-null default "0"
  manualSlPct: string; // integer percent, nullable
  // Adds (DCA)
  skipDca: boolean;
  /** UI holds a PERCENT of the entry size (100 = same size again); the
   *  wire field `dca_size_multiplier` stays a decimal multiplier. */
  dcaSizePct: string;
  // Markets
  marketFilterMode: string; // enum int, non-null default "0"
  marketFilterList: string; // comma/space list
  // Safety
  slippagePct: string; // percent (UI) → bps wire (×100), nullable
  drawdownStopPct: string; // integer percent, nullable
  // Entry ranges
  zoneStrategy: string; // enum int, non-null default "0"
}

// NOTE `tier_table_json` ("ramp-in tiers") is deliberately NOT part of
// the editable state (operator feedback R3 2026-07-06 — no v1
// equivalent, unclear semantics). The engine still reads the column;
// the editor simply never sends the field, so any stored table is
// preserved untouched (POST upsert + PATCH both keep omitted fields).

export const EMPTY_OVERRIDES: OverrideState = {
  enabled: true,
  sizingMode: "1",
  sizeLowPercent: "",
  sizeNormalPercent: "",
  sizeHighPercent: "",
  sizeLowUsd: "",
  sizeNormalUsd: "",
  sizeHighUsd: "",
  sizeBasis: "0",
  sizeMeaning: "0",
  leverageOverride: "",
  maxLeverage: "",
  maxPositionUsd: "",
  manualSlAction: "0",
  manualSlPct: "",
  skipDca: false,
  dcaSizePct: "",
  marketFilterMode: "0",
  marketFilterList: "",
  slippagePct: "",
  drawdownStopPct: "",
  zoneStrategy: "0",
};

// ─── seed from an existing subscription row ────────────────────────

const intStr = (v: number | null | undefined): string =>
  v === null || v === undefined ? "" : String(v);

/** Seed form state from a subscription row. The mode comes from the
 *  stored `sizing_mode`; the % tiers from the `size_*_percent` columns
 *  and the $ tiers from the `size_*_usd` columns. A legacy sub with only
 *  `size_usd_override` (and no $ tier columns) seeds that value into the
 *  Normal-$ tier so it stays visible. A stored trailing stop (action 2,
 *  no longer offered) collapses to "No stop" so the select stays
 *  coherent. */
export function overridesFromSub(sub: ExecSubscription): OverrideState {
  const slAction = sub.manual_sl_action ?? 0;
  return {
    enabled: sub.enabled,
    sizingMode: String(sub.sizing_mode ?? 1),
    sizeLowPercent: intStr(sub.size_low_percent),
    sizeNormalPercent: intStr(sub.size_normal_percent),
    sizeHighPercent: intStr(sub.size_high_percent),
    sizeLowUsd: sub.size_low_usd ?? "",
    // Fall back to the legacy single fixed-$ override for the Normal tier.
    sizeNormalUsd: sub.size_normal_usd ?? sub.size_usd_override ?? "",
    sizeHighUsd: sub.size_high_usd ?? "",
    sizeBasis: intStr(sub.size_basis ?? 0),
    sizeMeaning: intStr(sub.size_meaning ?? 0),
    leverageOverride:
      sub.leverage_override != null ? String(sub.leverage_override) : "",
    maxLeverage: intStr(sub.max_leverage),
    maxPositionUsd: sub.max_position_usd ?? "",
    manualSlAction: intStr(slAction === 1 ? 1 : 0),
    manualSlPct: intStr(sub.manual_sl_pct),
    skipDca: sub.skip_dca ?? false,
    dcaSizePct:
      sub.dca_size_multiplier != null && sub.dca_size_multiplier !== ""
        ? trimPct(Number(sub.dca_size_multiplier) * 100)
        : "",
    marketFilterMode: intStr(sub.market_filter_mode ?? 0),
    marketFilterList: (sub.market_filter_list ?? []).join(", "),
    slippagePct:
      sub.slippage_limit_bps === null || sub.slippage_limit_bps === undefined
        ? ""
        : trimPct(sub.slippage_limit_bps / 100),
    drawdownStopPct: intStr(sub.drawdown_stop_pct),
    zoneStrategy: intStr(sub.zone_strategy ?? 0),
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

/** Leverage override → clamped 1..125 int, or null when blank. */
const optLev = (s: string): number | null => {
  const n = optInt(s);
  if (n === null) return null;
  return Math.min(125, Math.max(1, n));
};

/** Build the override slice of a `CreateSubBody` from form state →
 *  `[partialBody, errors]`. ONLY the kept fields are ever set.
 *  `sizing_mode` is the explicit toggle; only the ACTIVE mode's three
 *  conviction tiers are sent (the inactive mode's keys are omitted,
 *  keeping stored values). Blank nullable fields are sent as explicit
 *  null so a blanked field genuinely clears on PATCH; on CREATE the same
 *  null falls back to the column default. Fields the form no longer
 *  shows (size multiplier, size cap, sizing_pct_equity, leverage cap,
 *  margin mode, TP close %, ramp-in tier table, size_usd_override) are
 *  never assigned and therefore never touched. */
export function overridesToBody(
  st: OverrideState,
): [Partial<CreateSubBody>, OverrideError[]] {
  const errors: OverrideError[] = [];
  const body: Partial<CreateSubBody> = {};

  body.enabled = st.enabled;

  // Size — explicit two-way mode. Send only the active mode's three
  // tiers so the inactive mode's stored columns survive untouched.
  const mode = enumInt(st.sizingMode); // 1 = % of account, 0 = $ per trade
  body.sizing_mode = mode;
  if (mode === 1) {
    body.size_low_percent = optInt(st.sizeLowPercent);
    body.size_normal_percent = optInt(st.sizeNormalPercent);
    body.size_high_percent = optInt(st.sizeHighPercent);
  } else {
    body.size_low_usd = optStr(st.sizeLowUsd);
    body.size_normal_usd = optStr(st.sizeNormalUsd);
    body.size_high_usd = optStr(st.sizeHighUsd);
  }
  // Account metric + size-meaning are always-on ints.
  body.size_basis = enumInt(st.sizeBasis);
  body.size_meaning = enumInt(st.sizeMeaning);

  // Leverage
  body.leverage_override = optLev(st.leverageOverride);
  body.max_leverage = optInt(st.maxLeverage);

  // Max position — clamps oversized orders down (does not skip).
  body.max_position_usd = optStr(st.maxPositionUsd);

  // Stop-loss fallback
  body.manual_sl_action = enumInt(st.manualSlAction);
  body.manual_sl_pct = optInt(st.manualSlPct);

  // Adds (DCA) — percent in the UI, decimal multiplier on the wire.
  {
    const pct = optNum(st.dcaSizePct);
    if (pct !== null && pct <= 0) {
      errors.push({ field: "dcaSizePct", message: "Add size must be above 0%." });
    }
    body.dca_size_multiplier = pct === null ? null : trimPct(pct / 100);
  }
  body.skip_dca = st.skipDca;

  // Markets
  body.market_filter_mode = enumInt(st.marketFilterMode);
  body.market_filter_list = st.marketFilterList
    .split(/[\s,]+/)
    .map((s) => s.trim().toUpperCase())
    .filter(Boolean);

  // Safety — slippage percent → bps (×100): 0.5% = 50 bps. Blank = keep default.
  body.slippage_limit_bps = pctToBps(st.slippagePct) ?? undefined;
  body.drawdown_stop_pct = optInt(st.drawdownStopPct);

  // Entry ranges
  body.zone_strategy = enumInt(st.zoneStrategy);

  // Light numeric sanity — the backend is the source of truth, and an
  // all-empty size is allowed (falls back to the caller default).
  if (enumInt(st.manualSlAction) === 1 && body.manual_sl_pct === null) {
    errors.push({
      field: "manualSlPct",
      message: "Set the stop distance % for the fixed stop.",
    });
  }

  return [body, errors];
}

// percent string → integer bps (×100). 0.5% → 50. Blank → null.
function pctToBps(s: string): number | null {
  const n = optNum(s);
  if (n === null) return null;
  return Math.round(n * 100);
}

// trim trailing-zero noise from a derived percent for display.
function trimPct(n: number): string {
  return String(Number(n.toFixed(4)));
}
