// Bot ↔ preset assignment editor — which of this user's Solana bots
// run a preset, plus the per-assignment overrides (buy size, TP/SL
// ladder) that beat the preset's own bot_config on that bot. Expands in
// place under the preset card (spec §D).
//
// Wire: PUT /api/trading/clients/{id}/presets/{preset_id} has PATCH
// semantics (omitted fields keep their value; clear_* flags remove an
// override). The ladder override still rides the LEGACY LegSpec[] shape
// on this route, so the shared ladder editor runs in its legacy dialect
// (no stop moves; sells are % of the armed position).

import { useEffect, useMemo, useState } from "react";
import { Save } from "lucide-react";
import { fmtSol, lamportsFromSolText, shortAddr } from "@degenbox/ui";
import {
  LadderSpecEditor,
  ladderDraftFromLegSpecs,
  legSpecsFromLadderDraft,
  validateLadderDraft,
  type LadderDraft,
} from "../../components/LadderSpecEditor";
import { Switch } from "../../components/ui";
import { Field, InlineEditor, NumField } from "../../components/form";
import {
  ipc,
  type AlphaPresetFull,
  type ClientInfo,
  type ClientPreset,
} from "./ipc";
import type { PresetAssignment } from "./ScannerPresetsSection";

interface Props {
  preset: AlphaPresetFull | null;
  /** Every Solana client known to the gateway (assignment targets). */
  clients: ClientInfo[];
  /** Current assignments OF THIS PRESET across those clients. */
  assignments: PresetAssignment[];
  onClose: () => void;
  /** Any mutation happened — the owner reloads the matrix. */
  onChanged: () => void;
}

export function AssignmentEditor({
  preset,
  clients,
  assignments,
  onClose,
  onChanged,
}: Props) {
  const [busyId, setBusyId] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);

  useEffect(() => {
    setErr(null);
    setBusyId(null);
    setExpanded(null);
  }, [preset?.id]);

  if (!preset) return null;

  const byClient = new Map(assignments.map((a) => [a.client.id, a.row]));

  const run = async (clientId: string, fn: () => Promise<unknown>) => {
    setBusyId(clientId);
    setErr(null);
    try {
      await fn();
      onChanged();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusyId(null);
    }
  };

  return (
    <InlineEditor
      anchored
      title={`Bots · ${preset.name}`}
      subtitle="Assigned bots buy when this preset fires. Overrides beat the preset's execution config on that bot only."
      onClose={onClose}
      footer={
        <button className="btn primary" onClick={onClose} style={{ marginLeft: "auto" }}>
          Done
        </button>
      }
    >
      <div className="grid gap-3">
        {err && <div className="error-box">{err}</div>}
        {clients.length === 0 && (
          <div className="empty" style={{ padding: "14px 4px" }}>
            No Solana bots registered with the gateway yet. Add a wallet in the
            Bots list above first.
          </div>
        )}
        {clients.map((c) => {
          const gid = c.gateway!.id;
          const row = byClient.get(c.id) ?? null;
          const busy = busyId !== null;
          return (
            <div
              key={c.id}
              className="rounded border border-line/15 bg-card/50 px-3 py-2.5"
            >
              <div className="flex items-center gap-2.5 flex-wrap">
                <Switch
                  on={row !== null}
                  disabled={busy}
                  title={row ? "Unassign (removes its overrides too)" : "Assign this bot"}
                  onToggle={(next) =>
                    run(c.id, () =>
                      next
                        ? ipc.clientPresetAssign(gid, preset.id, true)
                        : ipc.clientPresetUnassign(gid, preset.id),
                    )
                  }
                />
                <span className="text-[12.5px] font-medium">
                  {c.label?.trim() || shortAddr(c.address)}
                </span>
                <span className="mono text-[11px] text-ink-4">
                  {shortAddr(c.address, 5, 4)}
                </span>
                {c.paused && <span className="badge warn">bot paused</span>}
                {row !== null && (
                  <>
                    <span className={`badge ${row.enabled ? "ok" : ""}`}>
                      {row.enabled ? "running" : "assignment off"}
                    </span>
                    <span className="ml-auto flex items-center gap-2">
                      <Switch
                        on={row.enabled}
                        disabled={busy}
                        title="Pause/resume this assignment (overrides are kept)"
                        onToggle={(next) =>
                          run(c.id, () =>
                            ipc.clientPresetUpdate(gid, preset.id, { enabled: next }),
                          )
                        }
                      />
                      <button
                        className="btn xs"
                        disabled={busy}
                        onClick={() => setExpanded(expanded === c.id ? null : c.id)}
                      >
                        {expanded === c.id ? "Hide overrides" : "Overrides"}
                        {(row.buy_size_lamports_override != null ||
                          row.ladder_override) && (
                          <span className="text-accent">*</span>
                        )}
                      </button>
                    </span>
                  </>
                )}
              </div>
              {row !== null && expanded === c.id && (
                <OverridePanel
                  key={`${c.id}-${row.buy_size_lamports_override ?? "n"}-${
                    row.ladder_override?.length ?? "n"
                  }`}
                  gatewayId={gid}
                  presetId={preset.id}
                  row={row}
                  busy={busy}
                  onSave={(fn) => run(c.id, fn)}
                />
              )}
            </div>
          );
        })}
      </div>
    </InlineEditor>
  );
}

// ─── per-assignment override editor ────────────────────────────────

function OverridePanel({
  gatewayId,
  presetId,
  row,
  busy,
  onSave,
}: {
  gatewayId: string;
  presetId: string;
  row: ClientPreset;
  busy: boolean;
  onSave: (fn: () => Promise<unknown>) => void;
}) {
  const seededLadder = useMemo(
    () => ladderDraftFromLegSpecs(row.ladder_override),
    [row.ladder_override],
  );
  const [buySol, setBuySol] = useState(
    row.buy_size_lamports_override != null
      ? String(Number((row.buy_size_lamports_override / 1e9).toPrecision(12)))
      : "",
  );
  const [ladder, setLadder] = useState<LadderDraft>(seededLadder);
  const [localErr, setLocalErr] = useState<string | null>(null);

  const save = () => {
    setLocalErr(null);
    const lamports = buySol.trim() ? lamportsFromSolText(buySol) : null;
    if (buySol.trim() && (lamports == null || lamports <= 0)) {
      setLocalErr("the buy override must be above 0 SOL (or empty to clear it)");
      return;
    }
    const lErr = validateLadderDraft(ladder, "legacy");
    if (lErr) {
      setLocalErr(lErr);
      return;
    }
    const legs = legSpecsFromLadderDraft(ladder);
    onSave(() =>
      ipc.clientPresetUpdate(gatewayId, presetId, {
        ...(lamports != null
          ? { buy_size_lamports_override: lamports }
          : { clear_buy_size_lamports_override: true }),
        ...(legs.length > 0
          ? { ladder_override: legs }
          : { clear_ladder_override: true }),
      }),
    );
  };

  return (
    <div className="expand-in mt-2.5 pt-2.5 border-t border-line/10 grid gap-2.5">
      <NumField
        label="Buy override"
        unit="SOL"
        value={buySol}
        onChange={setBuySol}
        placeholder={
          row.buy_size_lamports_override != null
            ? fmtSol(row.buy_size_lamports_override)
            : "preset default"
        }
        hint="empty = use the preset's buy size"
      />
      <Field
        label="Ladder override"
        hint="no targets = fall back to the preset's own TP/SL"
      >
        <LadderSpecEditor
          dialect="legacy"
          value={ladder}
          onChange={setLadder}
          disabled={busy}
        />
      </Field>
      {localErr && <div className="error-box">{localErr}</div>}
      <div className="modal-foot">
        <button className="btn primary" disabled={busy} onClick={save}>
          <Save size={12} /> Save overrides
        </button>
      </div>
    </div>
  );
}
