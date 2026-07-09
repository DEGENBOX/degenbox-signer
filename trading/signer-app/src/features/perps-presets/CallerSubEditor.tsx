// Caller-subscription editor — the per-caller execution settings the
// bot applies to every parsed signal.
//
// v0.3.2 semantics rebuild (operator feedback R3, see
// docs/archive/perps-settings-semantics-2026-07-06.md): the MAIN editor
// shows exactly the settings that map 1:1 to how a call flows through
// the bot — a call says size / leverage / entries / TPs / SL, plus adds
// (DCA) and the markets it fires on; the user scales or caps those.
// Advanced keeps the justified power fields with honest names. "Ramp-in
// tiers" (tier_table_json) is gone from the UI entirely — the engine
// still reads the column and any stored table is preserved on save.
//
// Save wiring: a NEW follow rides `POST /api/exec/subscriptions`
// (upsert). An EXISTING follow rides `POST /api/exec/subscriptions/{id}`
// (PATCH semantics) so a blanked field is sent as explicit `null` and
// genuinely CLEARS — the upsert's coalesce can't clear anything. Fields
// this editor doesn't show are never sent, so they are never touched.

import { useEffect, useState } from "react";
import { ChevronRight, Save, Trash2 } from "lucide-react";
import { DangerConfirm, Segmented } from "../../components/ui";
import { CheckField, Field, FormGroup, InlineEditor, NumField, TextField } from "../../components/form";
import {
  deleteSub,
  friendlyGatewayError,
  patchSub,
  upsertSub,
  type CreateSubBody,
  type ExecSubscription,
  type ParserCaller,
  type PatchSubBody,
} from "./ipc";
import {
  EMPTY_OVERRIDES,
  MANUAL_SL_ACTION,
  MARGIN_MODE,
  overridesFromSub,
  overridesToBody,
  SIZE_BASIS,
  SIZE_MEANING,
  type OverrideError,
  type OverrideState,
} from "./callerOverrides";

interface Props {
  onClose: () => void;
  /** Saved / deleted — the owner reloads its list. */
  onSaved: () => void;
  /** The caller being configured (display info). */
  caller: ParserCaller | null;
  /** Existing perps-venue subscription, or null (first follow). */
  existing: ExecSubscription | null;
}

/** Small red line under a field when validation flags it. */
function FieldError({ msg }: { msg: string | undefined }) {
  if (!msg) return null;
  return <p className="field-row-hint text-down">{msg}</p>;
}

export function CallerSubEditor({ onClose, onSaved, caller, existing }: Props) {
  const [enabled, setEnabled] = useState(true);
  // Percent of the caller's size (wire: `size_multiplier`, 100% = 1.0).
  const [sizePct, setSizePct] = useState("100");
  const [sizeCapUsd, setSizeCapUsd] = useState("");
  const [leverageOverride, setLeverageOverride] = useState("");
  const [overrides, setOverrides] = useState<OverrideState>(EMPTY_OVERRIDES);
  const [showAdvanced, setShowAdvanced] = useState(false);
  const [fieldErrs, setFieldErrs] = useState<OverrideError[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleteErr, setDeleteErr] = useState<string | null>(null);

  // Seed the form on mount / when the target caller changes.
  useEffect(() => {
    setErr(null);
    setBusy(false);
    setFieldErrs([]);
    setConfirmDelete(false);
    setDeleteErr(null);
    setShowAdvanced(false);
    if (existing) {
      setEnabled(existing.enabled);
      const mult = Number(existing.size_multiplier);
      setSizePct(
        Number.isFinite(mult) ? String(Number((mult * 100).toFixed(2))) : "100",
      );
      setSizeCapUsd(existing.max_size_usd ?? "");
      setLeverageOverride(
        existing.leverage_override != null ? String(existing.leverage_override) : "",
      );
      setOverrides(overridesFromSub(existing));
    } else {
      setEnabled(true);
      setSizePct("100");
      setSizeCapUsd("");
      setLeverageOverride("");
      // A fresh follow leads with the percent-of-account tier model
      // (sizing_mode = 1) — mirrors the backend column default. Tiers stay
      // empty (placeholders) so the user types their own %s; no defaults.
      setOverrides({ ...EMPTY_OVERRIDES, sizingMode: "1" });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [caller?.caller_id, existing?.id]);

  if (!caller) return null;

  const setOv = <K extends keyof OverrideState>(key: K, value: OverrideState[K]) =>
    setOverrides((o) => ({ ...o, [key]: value }));

  const errFor = (field: keyof OverrideState): string | undefined =>
    fieldErrs.find((e) => e.field === field)?.message;

  const save = async () => {
    setErr(null);
    try {
      const pct = Number(sizePct);
      if (!Number.isFinite(pct) || pct <= 0 || pct > 1000) {
        throw new Error("size must be between 1 and 1000% (100 trades the caller's size)");
      }
      const mult = pct / 100;
      const lev = leverageOverride.trim();
      const levN = lev === "" ? null : Math.round(Number(lev));
      if (levN != null && (!Number.isFinite(levN) || levN < 1 || levN > 125)) {
        throw new Error("leverage must be 1 to 125, or empty to follow the caller");
      }
      const [overrideBody, errs] = overridesToBody(overrides);
      if (errs.length > 0) {
        setFieldErrs(errs);
        // Surface the first error inline AND open the disclosure so
        // the highlighted field is actually visible.
        setShowAdvanced(true);
        throw new Error(errs[0]?.message ?? "Fix the highlighted fields.");
      }
      setFieldErrs([]);

      const common: PatchSubBody = {
        ...overrideBody,
        enabled,
        size_multiplier: mult.toFixed(2),
        max_size_usd: sizeCapUsd.trim() === "" ? null : sizeCapUsd.trim(),
        leverage_override: levN,
      };
      setBusy(true);
      if (existing) {
        // PATCH: explicit nulls CLEAR blanked fields; anything this
        // editor doesn't know about (ramp-in tier table, legacy market
        // lists, client binding) is omitted and stays untouched.
        await patchSub(existing.id, common);
      } else {
        const body: CreateSubBody = {
          ...common,
          caller_id: caller.caller_id,
          venue: "hyperliquid",
        };
        await upsertSub(body);
      }
      onSaved();
      onClose();
    } catch (e) {
      setErr(friendlyGatewayError(e));
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async () => {
    if (!existing) return;
    setBusy(true);
    setDeleteErr(null);
    try {
      await deleteSub(existing.id);
      setConfirmDelete(false);
      onSaved();
      onClose();
    } catch (e) {
      setDeleteErr(friendlyGatewayError(e));
    } finally {
      setBusy(false);
    }
  };

  const pctSizing = overrides.sizingMode === "1";
  const slOn = overrides.manualSlAction !== "0";
  const filterOn = overrides.marketFilterMode !== "0";

  const callerDefaults = [
    caller.default_leverage != null ? `${caller.default_leverage}× leverage` : null,
    caller.default_size_usd != null ? `$${Number(caller.default_size_usd)} size` : null,
  ]
    .filter(Boolean)
    .join(", ");

  return (
    <>
      <InlineEditor
        anchored
        columns={2}
        title={
          existing
            ? `Caller settings: ${caller.display_name}`
            : `Follow ${caller.display_name}`
        }
        subtitle={
          <>
            How your Perpetuals bot executes {caller.display_name}'s calls. A call
            carries direction, size, leverage, entry, take-profits and stop — these
            settings scale or cap exactly that. Empty fields follow the call
            {callerDefaults ? ` (caller defaults: ${callerDefaults})` : ""}.
          </>
        }
        onClose={onClose}
        footer={
          <>
            {existing && (
              <button
                className="btn danger"
                disabled={busy}
                style={{ marginRight: "auto" }}
                onClick={() => setConfirmDelete(true)}
              >
                <Trash2 size={13} /> Unsubscribe…
              </button>
            )}
            <button className="btn" disabled={busy} onClick={onClose}>
              Cancel
            </button>
            <button className="btn primary" disabled={busy} onClick={save}>
              <Save size={13} />{" "}
              {busy ? "Saving…" : existing ? "Save settings" : "Start following"}
            </button>
          </>
        }
      >
        <FormGroup title="Following">
          <CheckField
            label="Following (executes new calls automatically while on)"
            checked={enabled}
            onChange={setEnabled}
          />
        </FormGroup>

        <FormGroup title="Size">
          <Field
            label="How to size each trade"
            hint={
              pctSizing
                ? "size every trade as a share of your own account — set a % per conviction tier below"
                : "scale or replace the dollar size the call carries"
            }
          >
            <Segmented
              options={[
                { value: "1", label: "% of account" },
                { value: "0", label: "$ per call" },
              ]}
              value={overrides.sizingMode}
              onChange={(v) => setOv("sizingMode", v)}
            />
          </Field>
          {pctSizing ? (
            <>
              <Field
                label="Size per conviction (% of account)"
                hint="the bot picks the tier from the call's conviction; a normal-conviction call uses Medium"
              >
                <div className="tier-pct-row">
                  <NumField
                    label="Small"
                    unit="%"
                    value={overrides.sizeLowPercent}
                    onChange={(t) => setOv("sizeLowPercent", t)}
                    inputMode="numeric"
                    placeholder="e.g. 2"
                  />
                  <NumField
                    label="Medium"
                    unit="%"
                    value={overrides.sizeNormalPercent}
                    onChange={(t) => setOv("sizeNormalPercent", t)}
                    inputMode="numeric"
                    placeholder="e.g. 5"
                  />
                  <NumField
                    label="High"
                    unit="%"
                    value={overrides.sizeHighPercent}
                    onChange={(t) => setOv("sizeHighPercent", t)}
                    inputMode="numeric"
                    placeholder="e.g. 10"
                  />
                </div>
              </Field>
              <FieldError
                msg={
                  errFor("sizeLowPercent") ??
                  errFor("sizeNormalPercent") ??
                  errFor("sizeHighPercent") ??
                  errFor("sizingPctEquity")
                }
              />
              <Field label="Account metric" hint="which balance the % is taken from">
                <Segmented
                  options={SIZE_BASIS.map((o) => ({
                    value: String(o.value),
                    label: o.label,
                  }))}
                  value={overrides.sizeBasis}
                  onChange={(v) => setOv("sizeBasis", v)}
                />
              </Field>
            </>
          ) : (
            <>
              <NumField
                label="Size"
                unit="%"
                value={sizePct}
                onChange={setSizePct}
                placeholder="100"
                hint="share of the call's size. 100 trades exactly what the call says, 50 halves it, 200 doubles it"
              />
              <NumField
                label="Fixed $ per trade"
                unit="$"
                value={overrides.sizeUsdOverride}
                onChange={(t) => setOv("sizeUsdOverride", t)}
                placeholder="use the call's size"
                hint="replaces the call's dollar size entirely. Leave empty to keep the call's"
              />
              <NumField
                label="Size cap"
                unit="$"
                value={sizeCapUsd}
                onChange={setSizeCapUsd}
                placeholder="no cap"
                hint="calls that would trade more than this are skipped entirely (not shrunk)"
              />
            </>
          )}
        </FormGroup>

        <FormGroup title="Leverage">
          <NumField
            label="Leverage"
            unit="×"
            value={leverageOverride}
            onChange={setLeverageOverride}
            inputMode="numeric"
            placeholder="follow the call"
            hint="replaces the leverage the call asks for. Leave empty to use the call's"
          />
        </FormGroup>

        <FormGroup title="Take-profits & stop-loss">
          <NumField
            label="Each take-profit closes"
            unit="%"
            value={overrides.tpClosePercent}
            onChange={(t) => setOv("tpClosePercent", t)}
            placeholder="33.33"
            hint="share of the position each of the call's TP targets closes. When the call names its own per-target sizes, those win"
          />
          <Field
            label="When a call has no stop-loss"
            hint={
              overrides.manualSlAction === "1"
                ? "place a stop a fixed distance below the entry"
                : overrides.manualSlAction === "2"
                  ? "place a stop that trails the price at this distance"
                  : "trade without a stop when the call doesn't carry one. Calls WITH a stop always use the call's stop"
            }
          >
            <Segmented
              options={MANUAL_SL_ACTION.map((o) => ({
                value: String(o.value),
                label: o.value === 0 ? "No stop" : o.label,
              }))}
              value={overrides.manualSlAction}
              onChange={(v) => setOv("manualSlAction", v)}
            />
          </Field>
          {slOn && (
            <>
              <NumField
                label="Stop distance"
                unit="%"
                value={overrides.manualSlPct}
                onChange={(t) => setOv("manualSlPct", t)}
                inputMode="numeric"
                placeholder="e.g. 10"
              />
              <FieldError msg={errFor("manualSlPct")} />
            </>
          )}
        </FormGroup>

        <FormGroup title="Markets">
          <Field
            label="Which markets to trade"
            hint={
              overrides.marketFilterMode === "1"
                ? "only calls for the listed markets execute"
                : overrides.marketFilterMode === "2"
                  ? "calls for the listed markets are skipped"
                  : "every market the caller calls"
            }
          >
            <Segmented
              options={[
                { value: "0", label: "All markets" },
                { value: "1", label: "Only these" },
                { value: "2", label: "All except" },
              ]}
              value={overrides.marketFilterMode}
              onChange={(v) => setOv("marketFilterMode", v)}
            />
          </Field>
          {filterOn && (
            <TextField
              label="Markets"
              value={overrides.marketFilterList}
              onChange={(t) => setOv("marketFilterList", t)}
              mono
              placeholder="BTC, ETH, SOL…"
            />
          )}
        </FormGroup>

        <FormGroup title="Adding to positions (DCA)">
          <CheckField
            label="Skip add-to-position calls"
            checked={overrides.skipDca}
            onChange={(v) => setOv("skipDca", v)}
            hint="ignore the caller's DCA calls; only fresh entries execute"
          />
          {!overrides.skipDca && (
            <>
              <NumField
                label="Add size"
                unit="%"
                value={overrides.dcaSizePct}
                onChange={(t) => setOv("dcaSizePct", t)}
                placeholder="100"
                hint="share of the entry size each add trades. 100 adds the same amount again, 50 adds half"
              />
              <FieldError msg={errFor("dcaSizePct")} />
            </>
          )}
        </FormGroup>

        <div style={{ gridColumn: "1 / -1" }}>
          <button
            type="button"
            className="btn sm"
            onClick={() => setShowAdvanced((s) => !s)}
          >
            <ChevronRight size={12} className={`chev ${showAdvanced ? "open" : ""}`} />
            {showAdvanced ? "Hide advanced settings" : "Advanced settings"}
          </button>
        </div>

        {showAdvanced && (
          <>
            <FormGroup title="Base size">
              {pctSizing && (
                <>
                  <NumField
                    label="Flat share fallback"
                    unit="%"
                    value={overrides.sizingPctEquity}
                    onChange={(t) => setOv("sizingPctEquity", t)}
                    placeholder="off"
                    hint="used when a call carries no conviction tag to match a tier above. Leave empty to rely on the Medium tier"
                  />
                  <FieldError msg={errFor("sizingPctEquity")} />
                </>
              )}
              <Field
                label="What the size counts as"
                hint={
                  overrides.sizeMeaning === "1"
                    ? "the size is the margin you post; the position is size × leverage"
                    : overrides.sizeMeaning === "2"
                      ? "the size is the most you can lose if the stop hits; position size is derived from the stop distance"
                      : "the size is the full position value (notional)"
                }
              >
                <Segmented
                  options={SIZE_MEANING.map((o) => ({
                    value: String(o.value),
                    label: o.label,
                  }))}
                  value={overrides.sizeMeaning}
                  onChange={(v) => setOv("sizeMeaning", v)}
                />
              </Field>
              <NumField
                label="Shrink oversized orders to"
                unit="$"
                value={overrides.maxPositionUsd}
                onChange={(t) => setOv("maxPositionUsd", t)}
                placeholder="off"
                hint="orders above this are shrunk to this amount instead of skipped (unlike the Size cap, which skips)"
              />
            </FormGroup>

            <FormGroup title="Leverage limits">
              <NumField
                label="Skip calls above"
                unit="×"
                value={overrides.maxLeverage}
                onChange={(t) => setOv("maxLeverage", t)}
                inputMode="numeric"
                placeholder="never skip"
                hint="calls asking for more leverage than this don't execute at all"
              />
              <NumField
                label="Lower leverage to at most"
                unit="×"
                value={overrides.leverageCap}
                onChange={(t) => setOv("leverageCap", t)}
                inputMode="numeric"
                placeholder="never lower"
                hint="higher leverage is lowered to this and the trade still executes"
              />
              <Field label="Margin mode">
                <Segmented
                  options={[
                    { value: "", label: "Account default" },
                    ...MARGIN_MODE.map((o) => ({
                      value: String(o.value),
                      label: o.label,
                    })),
                  ]}
                  value={overrides.marginMode}
                  onChange={(v) => setOv("marginMode", v)}
                />
              </Field>
            </FormGroup>

            <FormGroup title="Execution & safety">
              <NumField
                label="Max slippage"
                unit="%"
                value={overrides.slippagePct}
                onChange={(t) => setOv("slippagePct", t)}
                placeholder="0.5"
                hint="market orders won't fill at a worse price than this"
              />
              <NumField
                label="Drawdown pause"
                unit="%"
                value={overrides.drawdownStopPct}
                onChange={(t) => setOv("drawdownStopPct", t)}
                inputMode="numeric"
                placeholder="off"
                hint="pauses this follow once your account is down this much since you enrolled. Open positions stay open"
              />
            </FormGroup>

            <FormGroup title="Entry ranges">
              <Field
                label="When the call's entry is a price range"
                hint="how many limit orders the range is split into: one at the midpoint, one at each edge, or edges plus the middle"
              >
                <Segmented
                  options={[
                    { value: "0", label: "1 order (midpoint)" },
                    { value: "1", label: "2 orders (edges)" },
                    { value: "2", label: "3 orders" },
                  ]}
                  value={overrides.zoneStrategy}
                  onChange={(v) => setOv("zoneStrategy", v)}
                />
              </Field>
            </FormGroup>
          </>
        )}

        {err && <div className="error-box">{err}</div>}
      </InlineEditor>

      <DangerConfirm
        open={confirmDelete}
        title="Unsubscribe from caller"
        phrase="unsubscribe"
        busy={busy}
        error={deleteErr}
        onCancel={() => setConfirmDelete(false)}
        onConfirm={doDelete}
      >
        <p style={{ marginTop: 0 }}>
          Stop executing <strong>{caller.display_name}</strong>'s calls and delete
          your settings for this caller. Open positions stay untouched.
        </p>
      </DangerConfirm>
    </>
  );
}
