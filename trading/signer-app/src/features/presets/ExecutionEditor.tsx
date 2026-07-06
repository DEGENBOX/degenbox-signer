// Execution + sell-strategy editor for one scanner preset — the ONLY
// thing the app edits on a preset (filters stay website-owned).
// Expands in place, right under the preset it belongs to (spec §D).
//
// v0.3.0 slice 9: the sell strategy is the shared LadderSpec v2 editor
// (spec §E) — fully custom targets that sell a % of what's still open,
// each optionally moving the stop (stop-loss ladder), plus one base
// stop loss. No premade presets. Saved as `bot_config.ladder`, the key
// bot sessions actually compile at start; the dead legacy
// `take_profits`/`stop_loss_pct` keys are stripped on save.

import { useEffect, useMemo, useState } from "react";
import { Save } from "lucide-react";
import { lamportsFromSolText } from "@degenbox/ui";
import {
  EMPTY_LADDER,
  LadderSpecEditor,
  draftFromLadderSpec,
  ladderDraftFromLegacyBotConfig,
  ladderSpecFromDraft,
  validateLadderDraft,
  type LadderDraft,
} from "../../components/LadderSpecEditor";
import { Field, FormGroup, InlineEditor, NumField } from "../../components/form";
import { patchPresetBotConfig, solText, type AlphaPresetFull } from "./ipc";
import { hasExecutionConfig, mergeBotConfig, parseBotConfig } from "./botConfig";

interface Props {
  preset: AlphaPresetFull | null;
  onClose: () => void;
  onSaved: () => void;
}

export function ExecutionEditor({ preset, onClose, onSaved }: Props) {
  const [buySol, setBuySol] = useState("");
  const [slipPct, setSlipPct] = useState("");
  const [tipSol, setTipSol] = useState("");
  const [maxConc, setMaxConc] = useState("");
  const [ladder, setLadder] = useState<LadderDraft>(EMPTY_LADDER);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const existing = useMemo(
    () => (preset ? parseBotConfig(preset.bot_config) : null),
    [preset],
  );

  // Seed on mount / when the target preset changes.
  useEffect(() => {
    if (!preset || !existing) return;
    setErr(null);
    setBusy(false);
    setBuySol(existing.buySizeLamports != null ? solText(existing.buySizeLamports) : "");
    setSlipPct(existing.slippageBps != null ? String(existing.slippageBps / 100) : "");
    setTipSol(existing.tipLamports != null ? solText(existing.tipLamports) : "");
    setMaxConc(existing.maxConcurrent != null ? String(existing.maxConcurrent) : "");
    // Prefer the canonical v2 ladder; fall back to a one-time import of
    // the dead legacy keys so old configs don't look empty.
    setLadder(
      draftFromLadderSpec(existing.ladder) ??
        ladderDraftFromLegacyBotConfig(existing.takeProfits, existing.stopLossPct),
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [preset?.id]);

  if (!preset) return null;

  const buildConfig = () => {
    const buy = buySol.trim() ? lamportsFromSolText(buySol) : null;
    if (buySol.trim() && (buy == null || buy <= 0)) {
      throw new Error("the buy size must be above 0 SOL (or empty to unset)");
    }
    const slip = slipPct.trim() ? Number(slipPct) : null;
    if (slip != null && (!Number.isFinite(slip) || slip <= 0 || slip > 100)) {
      throw new Error("slippage must be between 0 and 100%");
    }
    const tip = tipSol.trim() ? lamportsFromSolText(tipSol) : null;
    if (tipSol.trim() && (tip == null || tip < 0)) {
      throw new Error("the tip can't be negative");
    }
    const conc = maxConc.trim() ? Number(maxConc) : null;
    if (conc != null && (!Number.isInteger(conc) || conc < 1)) {
      throw new Error("max open positions must be a whole number of at least 1");
    }
    const lErr = validateLadderDraft(ladder, "v2");
    if (lErr) throw new Error(lErr);
    return {
      buySizeLamports: buy,
      slippageBps: slip != null ? Math.round(slip * 100) : null,
      tipLamports: tip,
      maxConcurrent: conc,
      ladder: ladderSpecFromDraft(ladder),
    };
  };

  const save = async (cfg?: ReturnType<typeof buildConfig>) => {
    setErr(null);
    try {
      const next = cfg ?? buildConfig();
      setBusy(true);
      await patchPresetBotConfig(preset.id, mergeBotConfig(preset.bot_config, next));
      onSaved();
      onClose();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const clearAll = () =>
    save({
      buySizeLamports: null,
      slippageBps: null,
      tipLamports: null,
      maxConcurrent: null,
      ladder: null,
    });

  return (
    <InlineEditor
      anchored
      columns={2}
      title={`Execution · ${preset.name}`}
      subtitle="What your bots do when this preset fires. Empty fields fall back to the defaults; per-bot overrides live under each bot."
      onClose={onClose}
      footer={
        <>
          {existing && hasExecutionConfig(existing) && (
            <button
              className="btn danger"
              disabled={busy}
              style={{ marginRight: "auto" }}
              onClick={clearAll}
              title="Remove the execution config. The preset goes back to alerts only"
            >
              Clear (alerts only)
            </button>
          )}
          <button className="btn" disabled={busy} onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy} onClick={() => save()}>
            <Save size={13} /> {busy ? "Saving…" : "Save execution"}
          </button>
        </>
      }
    >
      <FormGroup title="Buying">
        <NumField
          label="Buy size"
          unit="SOL"
          value={buySol}
          onChange={setBuySol}
          placeholder="0.1"
          hint="spent per signal, required for auto-buying"
        />
        <NumField
          label="Max open positions"
          value={maxConc}
          onChange={setMaxConc}
          inputMode="numeric"
          placeholder="unlimited"
          hint="open positions from this preset at the same time"
        />
      </FormGroup>

      <FormGroup title="Execution">
        <NumField label="Slippage" unit="%" value={slipPct} onChange={setSlipPct} placeholder="2" />
        <NumField
          label="Priority tip"
          unit="SOL"
          value={tipSol}
          onChange={setTipSol}
          placeholder="0.0005"
          hint="paid per swap to land faster"
        />
      </FormGroup>

      {/* Full width: the ladder rows need the room to stay on one line. */}
      <div style={{ gridColumn: "1 / -1" }}>
        <FormGroup title="Selling: take-profit & stop ladder">
          <Field
            label="Targets"
            hint="Each target sells a share of what's still open, and can move the stop once it fills. Build the exit exactly how you want it."
          >
            <LadderSpecEditor value={ladder} onChange={setLadder} disabled={busy} />
          </Field>
        </FormGroup>
      </div>

      {err && <div className="error-box">{err}</div>}
    </InlineEditor>
  );
}
