// Emergency flatten — the module-header money kill. Big, unambiguous,
// type-to-confirm. Closes 100% of every open position on the active
// venue through the command layer (reduce-only for perps; on-chain
// balance-clamped sells for Solana). Per-position results are surfaced;
// if any leg fails the modal stays open so nothing is silently missed.

import { useState } from "react";
import { DangerConfirm } from "./ui";
import { commands, type FlattenResult } from "../lib/commands";
import type { Mode } from "../styles/mode";

export function EmergencyFlatten({
  mode,
  onDone,
}: {
  mode: Mode;
  /** Called with a human summary once the flatten fully succeeds. */
  onDone: (summary: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [results, setResults] = useState<FlattenResult[] | null>(null);

  const venue = mode === "sol" ? "Solana" : "Perpetuals";

  const run = async () => {
    setBusy(true);
    setErr(null);
    try {
      const res = mode === "sol"
        ? await commands.sol.flatten()
        : await commands.perps.flatten();
      setResults(res);
      const failed = res.filter((r) => !r.ok);
      if (res.length === 0) {
        onDone(`No open ${venue} positions. Nothing to flatten.`);
        close();
      } else if (failed.length === 0) {
        onDone(`Flattened ${res.length} ${venue} position${res.length === 1 ? "" : "s"}.`);
        close();
      } else {
        // Keep the modal open showing which legs failed.
        setErr(
          `${failed.length} of ${res.length} did not close. Review below and retry.`,
        );
      }
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const close = () => {
    setOpen(false);
    setErr(null);
    setResults(null);
  };

  return (
    <>
      <button
        className="btn flatten-btn"
        onClick={() => setOpen(true)}
        title={`Close ALL open ${venue} positions immediately`}
      >
        Flatten {venue}
      </button>
      <DangerConfirm
        open={open}
        title={`Flatten all ${venue} positions`}
        phrase="FLATTEN"
        busy={busy}
        error={err}
        onCancel={close}
        onConfirm={run}
      >
        <p style={{ marginTop: 0 }}>
          This immediately closes <strong>100% of every open {venue} position</strong>{" "}
          on this device.{" "}
          {mode === "sol"
            ? "Each sells for SOL through this device's signer, clamped to the on-chain balance; the holding wallet is resolved on-chain (ambiguous ones are refused, not guessed)."
            : "Each closes reduce-only through this device's signer at live size."}{" "}
          Auto-exec keeps running afterwards. Pause the device first if you also
          want to stop new entries.
        </p>
        {results && results.length > 0 && (
          <div className="flatten-results">
            {results.map((r) => (
              <div key={r.label} className={`flatten-row ${r.ok ? "ok" : "fail"}`}>
                <span className="flatten-sym">{r.label}</span>
                <span className="flatten-detail">{r.ok ? r.detail : r.detail}</span>
              </div>
            ))}
          </div>
        )}
      </DangerConfirm>
    </>
  );
}
