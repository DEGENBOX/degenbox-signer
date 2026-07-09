// Caller-subscription editor — the per-caller execution settings the
// bot applies to every parsed signal.
//
// Flat-form rebuild (operator feedback, 2026-07): every section is
// always visible in one column — no Advanced disclosure, one Save. The
// editor shows exactly the settings that map 1:1 to how a call flows
// through the bot: size (three conviction tiers as a share of your
// account, or a fixed $ instead), leverage, max position, the stop to
// use when a call carries none, DCA, markets, safety and how entry
// ranges fan out. Take-profits have no field — they split equally
// across the call's targets and caller-named per-target sizes win.
//
// Fields that duplicated or muddied those (size multiplier, size cap,
// share-of-account %, size basis/meaning, leverage cap, margin mode,
// per-TP close %, trailing stop) are gone from the UI AND the save
// payload — their keys are never sent, so on an existing follow PATCH
// omits them and the stored column stays untouched.
//
// Save wiring: a NEW follow rides `POST /api/exec/subscriptions`
// (upsert). An EXISTING follow rides `POST /api/exec/subscriptions/{id}`
// (PATCH semantics) so a blanked field is sent as explicit `null` and
// genuinely CLEARS — the upsert's coalesce can't clear anything. Fields
// this editor doesn't show are never sent, so they are never touched.

import { useEffect, useState } from "react";
import { Save, Trash2 } from "lucide-react";
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
  overridesFromSub,
  overridesToBody,
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
  const [overrides, setOverrides] = useState<OverrideState>(EMPTY_OVERRIDES);
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
    setOverrides(existing ? overridesFromSub(existing) : EMPTY_OVERRIDES);
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
      const [overrideBody, errs] = overridesToBody(overrides);
      if (errs.length > 0) {
        setFieldErrs(errs);
        throw new Error(errs[0]?.message ?? "Fix the highlighted fields.");
      }
      setFieldErrs([]);

      const common: PatchSubBody = { ...overrideBody };
      setBusy(true);
      if (existing) {
        // PATCH: explicit nulls CLEAR blanked fields; anything this
        // editor doesn't know about (ramp-in tier table, size cap /
        // multiplier, margin mode, client binding) is omitted and
        // stays untouched.
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

  const pctMode = overrides.sizingMode === "1";
  const slOn = overrides.manualSlAction !== "0";
  const filterOn = overrides.marketFilterMode !== "0";
  const sizeMeaningHint =
    overrides.sizeMeaning === "1"
      ? "Amount is the collateral. Position = margin × leverage."
      : overrides.sizeMeaning === "2"
        ? "Amount is the max loss if the stop is hit."
        : "Amount is the full position value.";

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
            checked={overrides.enabled}
            onChange={(v) => setOv("enabled", v)}
          />
        </FormGroup>

        <FormGroup title="Size">
          <Field
            label="How to size"
            hint={
              pctMode
                ? "each tier is a share of your account"
                : "each tier is a fixed dollar amount per trade"
            }
          >
            <Segmented
              options={[
                { value: "1", label: "% of account" },
                { value: "0", label: "$ per trade" },
              ]}
              value={overrides.sizingMode}
              onChange={(v) => setOv("sizingMode", v)}
            />
          </Field>

          <Field
            label={
              pctMode
                ? "Size per conviction (% of account)"
                : "Size per conviction ($ per trade)"
            }
            hint="the bot picks the tier from the call's conviction; a normal-conviction call uses Normal. Empty Small / High fall back to Normal"
          >
            <div className="tier-pct-row">
              {pctMode ? (
                <>
                  <NumField
                    label="Small"
                    unit="%"
                    value={overrides.sizeLowPercent}
                    onChange={(t) => setOv("sizeLowPercent", t)}
                    inputMode="numeric"
                    placeholder="e.g. 1"
                  />
                  <NumField
                    label="Normal"
                    unit="%"
                    value={overrides.sizeNormalPercent}
                    onChange={(t) => setOv("sizeNormalPercent", t)}
                    inputMode="numeric"
                    placeholder="e.g. 2"
                  />
                  <NumField
                    label="High"
                    unit="%"
                    value={overrides.sizeHighPercent}
                    onChange={(t) => setOv("sizeHighPercent", t)}
                    inputMode="numeric"
                    placeholder="e.g. 5"
                  />
                </>
              ) : (
                <>
                  <NumField
                    label="Small"
                    unit="$"
                    value={overrides.sizeLowUsd}
                    onChange={(t) => setOv("sizeLowUsd", t)}
                    placeholder="e.g. 50"
                  />
                  <NumField
                    label="Normal"
                    unit="$"
                    value={overrides.sizeNormalUsd}
                    onChange={(t) => setOv("sizeNormalUsd", t)}
                    placeholder="e.g. 100"
                  />
                  <NumField
                    label="High"
                    unit="$"
                    value={overrides.sizeHighUsd}
                    onChange={(t) => setOv("sizeHighUsd", t)}
                    placeholder="e.g. 250"
                  />
                </>
              )}
            </div>
          </Field>

          {pctMode && (
            <Field
              label="Account metric"
              hint="which account balance the % is measured against"
            >
              <Segmented
                options={[
                  { value: "0", label: "Equity" },
                  { value: "1", label: "Balance" },
                  { value: "2", label: "Available" },
                ]}
                value={overrides.sizeBasis}
                onChange={(v) => setOv("sizeBasis", v)}
              />
            </Field>
          )}

          <Field label="What the size means" hint={sizeMeaningHint}>
            <Segmented
              options={[
                { value: "0", label: "Position size" },
                { value: "1", label: "Margin" },
                { value: "2", label: "SL risk" },
              ]}
              value={overrides.sizeMeaning}
              onChange={(v) => setOv("sizeMeaning", v)}
            />
          </Field>
        </FormGroup>

        <FormGroup title="Leverage">
          <NumField
            label="Leverage"
            unit="×"
            value={overrides.leverageOverride}
            onChange={(t) => setOv("leverageOverride", t)}
            inputMode="numeric"
            placeholder="follow the call"
            hint="replaces the leverage the call asks for. Leave empty to use the call's"
          />
          <NumField
            label="Skip calls above"
            unit="×"
            value={overrides.maxLeverage}
            onChange={(t) => setOv("maxLeverage", t)}
            inputMode="numeric"
            placeholder="never skip"
            hint="calls asking for more leverage than this don't execute at all"
          />
        </FormGroup>

        <FormGroup title="Max position">
          <NumField
            label="Shrink oversized orders to"
            unit="$"
            value={overrides.maxPositionUsd}
            onChange={(t) => setOv("maxPositionUsd", t)}
            placeholder="off"
            hint="orders above this are shrunk to this amount and still execute — they are not skipped"
          />
        </FormGroup>

        <FormGroup title="Take-profits">
          <p className="field-row-hint" style={{ marginTop: 0 }}>
            Take-profits split equally across the call's targets; caller-named
            per-target sizes win.
          </p>
        </FormGroup>

        <FormGroup title="Stop fallback">
          <Field
            label="When a call has no stop-loss"
            hint={
              overrides.manualSlAction === "1"
                ? "place a stop a fixed distance below the entry"
                : "trade without a stop when the call doesn't carry one. Calls WITH a stop always use the call's stop"
            }
          >
            <Segmented
              options={[
                { value: "0", label: "No stop" },
                { value: "1", label: "Fixed %" },
              ]}
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

        <FormGroup title="Safety">
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
