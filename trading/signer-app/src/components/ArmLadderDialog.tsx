// Arm/edit a TP/SL ladder on a Solana position — app-side port of the
// web's ArmTargetDialog. PUT replaces any live ladder atomically; leg
// fractions anchor to the position size at arm time. The dialog also
// offers Disarm when a live ladder exists.

import { useEffect, useState } from "react";
import { Crosshair, ShieldOff } from "lucide-react";
import { ipc, type PositionTargetRow } from "../ipc";
import { Modal, shortAddr } from "./ui";
import {
  LadderEditor,
  legsFromTarget,
  toLegSpecs,
  validateEditableLadder,
  type EditableLeg,
} from "./LadderEditor";

interface Props {
  open: boolean;
  onClose: () => void;
  /** Armed / replaced / disarmed — owner refreshes its target list. */
  onChanged: () => void;
  mint: string;
  symbol: string;
  /** Entry autofill (live price); user can override. */
  suggestedEntryUsd: string | null;
  /** Live ladder when one is armed — pre-fills + enables Disarm. */
  existing: PositionTargetRow | null;
}

export function ArmLadderDialog({
  open,
  onClose,
  onChanged,
  mint,
  symbol,
  suggestedEntryUsd,
  existing,
}: Props) {
  const [entry, setEntry] = useState("");
  const [legs, setLegs] = useState<EditableLeg[]>([]);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setErr(null);
    setBusy(false);
    if (existing) {
      setEntry(existing.entry_price_usd);
      setLegs(legsFromTarget(existing));
    } else {
      setEntry(suggestedEntryUsd ?? "");
      setLegs([]);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, mint, existing?.id]);

  const entryNum = entry.trim() === "" ? null : Number(entry);
  const entryError =
    entryNum == null || !Number.isFinite(entryNum) || entryNum <= 0
      ? "entry price must be > 0"
      : null;
  const ladderError = validateEditableLadder(legs);
  const valid = entryError == null && ladderError == null;

  const arm = async () => {
    if (!valid) return;
    setBusy(true);
    setErr(null);
    try {
      await ipc.solTargetArm(mint, entry.trim(), toLegSpecs(legs));
      onChanged();
      onClose();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const disarm = async () => {
    setBusy(true);
    setErr(null);
    try {
      await ipc.solTargetDisarm(mint);
      onChanged();
      onClose();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open={open}
      onClose={onClose}
      title={
        <>
          <Crosshair size={14} style={{ verticalAlign: "-2px" }} />{" "}
          {existing ? "Edit TP/SL ladder" : "Arm TP/SL ladder"} · {symbol}{" "}
          <span className="mono" style={{ color: "var(--fg-faint)", fontSize: 12 }}>
            {shortAddr(mint, 4, 4)}
          </span>
        </>
      }
      width={520}
      locked={busy}
    >
      <div style={{ display: "grid", gap: 14 }}>
        <div className="field-group">
          <label className="field">Entry price (USD)</label>
          <input
            className="input mono"
            inputMode="decimal"
            value={entry}
            placeholder="0.0000"
            disabled={busy}
            onChange={(e) => setEntry(e.target.value)}
          />
          <div style={{ fontSize: 11, color: "var(--fg-faint)", marginTop: 4 }}>
            snapshot at arm-time. All leg levels are computed against this
          </div>
        </div>

        <div className="field-group">
          <label className="field">Ladder</label>
          <LadderEditor value={legs} onChange={setLegs} disabled={busy} />
          <div style={{ fontSize: 11, color: "var(--fg-faint)", marginTop: 4 }}>
            each TP leg sells its fraction of the position (as held at arm time)
            when its level hits; the SL exits the remainder and cancels open TP legs
          </div>
        </div>

        {legs.length > 0 && ladderError && (
          <div className="error-box">{ladderError}</div>
        )}
        {entry.trim() !== "" && entryError && <div className="error-box">{entryError}</div>}
        {err && <div className="error-box">{err}</div>}

        <div className="modal-foot" style={{ marginTop: 0 }}>
          {existing && (
            <button
              className="btn danger"
              style={{ marginRight: "auto" }}
              disabled={busy}
              title="Cancel the live ladder. The position stays open, unprotected"
              onClick={disarm}
            >
              <ShieldOff size={13} /> Disarm
            </button>
          )}
          <button className="btn" disabled={busy} onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy || !valid} onClick={arm}>
            {busy ? "Working…" : existing ? "Replace ladder" : "Arm ladder"}
          </button>
        </div>
      </div>
    </Modal>
  );
}
