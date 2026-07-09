// Per-caller override schema + (de)serialization for the caller-sub
// editor. App-side adaptation of the web's caller override module
// (frontend/modules/alpha-scanner/src/callers/callerOverrides.ts) —
// copied rather than imported because the signer-app workspace does
// not depend on the alpha-scanner module. Field names, units and enum
// variants mirror the Rust `CreateSubReq`
// (crates/modules/execution-computer/src/domain.rs); bps columns are
// surfaced as human percents and converted at the wire boundary here.
//
// Flat-form rebuild (operator feedback, 2026-07): the editor shows one
// always-visible column of the settings that map 1:1 to how a call
// flows through the bot — sizing tiers, leverage, max position, stop
// fallback, DCA, markets, safety, entry ranges. The power fields that
// duplicated or muddied those (size_multiplier, max_size_usd,
// sizing_pct_equity, size_basis, size_meaning, leverage_cap,
// margin_mode, tp_close_percent) are gone from BOTH the UI and the
// save payload — their keys are simply never sent, so on an existing
// sub PATCH omits them and the stored column is left untouched.

import type { CreateSubBody, ExecSubscription } from "./ipc";

// ─── enum variants (mirror computer.rs constants) ──────────────────

/** `sizing_mode` — how the base size is derived. Not a user toggle
 * anymore: it is DERIVED at save time from whether a fixed $ per trade
 * was entered (0 = fixed USD, 1 = percent-of-account tiers). */
export const SIZING_MODE = [
  { value: 0, label: "Fixed USD" },
  { value: 1, label: "% of account" },
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
 * Trailing (2) is still honoured by the engine but is no longer an
 * option in this editor; only No stop / Fixed % are offered. */
export const MANUAL_SL_ACTION = [
  { value: 0, label: "No stop" },
  { value: 1, label: "Fixed %" },
] as const;

// ─── editable form state ───────────────────────────────────────────

/** String-typed mirror of every editable override. Empty string =
 * unset ("inherit caller default" / keep backend default). */
export interface OverrideState {
  // Sizing — headline is the three conviction tiers; the fixed $ is the
  // small "instead" secondary. Which one wins is derived on save.
  sizeUsdOverride: string; // USD, nullable → sizing_mode 0 when filled
  sizeLowPercent: string; // integer percent, nullable
  sizeNormalPercent: string; // integer percent, nullable
  sizeHighPercent: string; // integer percent, nullable
  maxPositionUsd: string; // USD, nullable — shrink clamp
  // Leverage
  maxLeverage: string; // hard reject ceiling, nullable
  // Risk / SL
  manualSlAction: string; // enum int, non-null default "0"
  manualSlPct: string; // integer percent, nullable
  zoneStrategy: string; // enum int, non-null default "0"
  slippagePct: string; // percent (UI) → bps wire (×100)
  drawdownStopPct: string; // integer percent, nullable
  // DCA
  /** UI holds a PERCENT of the entry size (100 = same size); the wire
   * field `dca_size_multiplier` stays a decimal multiplier. */
  dcaSizePct: string;
  skipDca: boolean;
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
  sizeUsdOverride: "",
  sizeLowPercent: "",
  sizeNormalPercent: "",
  sizeHighPercent: "",
  maxPositionUsd: "",
  maxLeverage: "",
  manualSlAction: "0",
  manualSlPct: "",
  zoneStrategy: "0",
  slippagePct: "",
  drawdownStopPct: "",
  dcaSizePct: "",
  skipDca: false,
  marketFilterMode: "0",
  marketFilterList: "",
};

// ─── seed from an existing subscription row ────────────────────────

const intStr = (v: number | null | undefined): string =>
  v === null || v === undefined ? "" : String(v);

/** Seed override state from a subscription row. Tiers come from the
 * tier columns; the fixed $ is only surfaced when the stored row was
 * in fixed-USD mode (`sizing_mode === 0`). Non-null enum columns fall
 * back to "0"; nullable columns become "" ("inherit"). */
export function overridesFromSub(sub: ExecSubscription): OverrideState {
  return {
    sizeUsdOverride: sub.sizing_mode === 0 ? (sub.size_usd_override ?? "") : "",
    sizeLowPercent: intStr(sub.size_low_percent),
    sizeNormalPercent: intStr(sub.size_normal_percent),
    sizeHighPercent: intStr(sub.size_high_percent),
    maxPositionUsd: sub.max_position_usd ?? "",
    maxLeverage: intStr(sub.max_leverage),
    manualSlAction: intStr(sub.manual_sl_action ?? 0),
    manualSlPct: intStr(sub.manual_sl_pct),
    zoneStrategy: intStr(sub.zone_strategy ?? 0),
    slippagePct:
      sub.slippage_limit_bps === null || sub.slippage_limit_bps === undefined
        ? ""
        : trimPct(sub.slippage_limit_bps / 100),
    drawdownStopPct: intStr(sub.drawdown_stop_pct),
    dcaSizePct:
      sub.dca_size_multiplier != null && sub.dca_size_multiplier !== ""
        ? trimPct(Number(sub.dca_size_multiplier) * 100)
        : "",
    skipDca: sub.skip_dca ?? false,
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
 * `sizing_mode` is DERIVED: a filled "fixed $ per trade" saves
 * fixed-USD mode (0), otherwise the percent-of-account tiers win (1).
 * On CREATE (POST upsert) a null falls back to the column default; on
 * EDIT the same body rides PATCH where an explicit null CLEARS the
 * column — so blanking a field genuinely unsets it. Fields the editor
 * no longer shows (size multiplier, size cap, size basis/meaning,
 * leverage cap, margin mode, TP close %, ramp-in tier table) are never
 * sent and therefore never touched. */
export function overridesToBody(
  st: OverrideState,
): [Partial<CreateSubBody>, OverrideError[]] {
  const errors: OverrideError[] = [];
  const body: Partial<CreateSubBody> = {};

  // Sizing — fixed $ filled → fixed-USD mode; else tier percents.
  const fixedUsd = optStr(st.sizeUsdOverride);
  body.sizing_mode = fixedUsd !== null ? 0 : 1;
  body.size_usd_override = fixedUsd;
  body.size_low_percent = optInt(st.sizeLowPercent);
  body.size_normal_percent = optInt(st.sizeNormalPercent);
  body.size_high_percent = optInt(st.sizeHighPercent);
  body.max_position_usd = optStr(st.maxPositionUsd);

  // Leverage
  body.max_leverage = optInt(st.maxLeverage);

  // Risk / SL
  body.manual_sl_action = enumInt(st.manualSlAction);
  body.manual_sl_pct = optInt(st.manualSlPct);
  body.zone_strategy = enumInt(st.zoneStrategy);
  body.slippage_limit_bps = pctToBps(st.slippagePct) ?? undefined;
  body.drawdown_stop_pct = optInt(st.drawdownStopPct);

  // DCA
  {
    // Percent in the UI, decimal multiplier on the wire (100% → "1").
    const pct = optNum(st.dcaSizePct);
    if (pct !== null && pct <= 0) {
      errors.push({ field: "dcaSizePct", message: "Add size must be above 0%." });
    }
    body.dca_size_multiplier = pct === null ? null : trimPct(pct / 100);
  }
  body.skip_dca = st.skipDca;

  // Markets filter
  body.market_filter_mode = enumInt(st.marketFilterMode);
  body.market_filter_list = st.marketFilterList
    .split(/[\s,]+/)
    .map((s) => s.trim().toUpperCase())
    .filter(Boolean);

  // Light numeric sanity — the backend is the source of truth, and an
  // all-empty size is allowed (falls back to the caller default).
  if (enumInt(st.manualSlAction) !== 0 && body.manual_sl_pct === null) {
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
