// Circuit-breaker banner — the ONLY surface that can clear a tripped
// consecutive-loss breaker now that the web dashboard is read-only.
//
// A tripped scope halts ALL auto-exec server-side; before this banner
// existed that halt was invisible in the bot (the reset control lived
// only in the stripped dashboard). Hidden while the account is clean;
// amber while losses accumulate; red + Reset when tripped so a halted
// account can't be missed on the live view.
//
// Polls the gateway on the app's standard useEffect+setInterval cadence
// (the signer-app has no react-query); after a reset it re-polls to
// reflect the cleared state immediately.

import { useCallback, useEffect, useMemo, useState } from "react";
import { AlertTriangle } from "lucide-react";
import { DangerConfirm } from "./ui";
import {
  circuitBreakerReset,
  circuitBreakerStatus,
  type CircuitBreakerScope,
  type CircuitBreakerStatus,
} from "../features/perps-presets/ipc";

const POLL_MS = 15_000;

export function CircuitBreakerBanner() {
  const [data, setData] = useState<CircuitBreakerStatus | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  // Fire-and-forget refresh used after a manual reset.
  const refresh = useCallback(
    () => circuitBreakerStatus().then(setData, () => {}),
    [],
  );

  useEffect(() => {
    let alive = true;
    const load = () =>
      circuitBreakerStatus().then((s) => alive && setData(s), () => {});
    load();
    const id = setInterval(load, POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  // Worst scope = any tripped+enabled one first, else the enabled scope
  // closest to its threshold.
  const worst = useMemo<CircuitBreakerScope | null>(() => {
    if (!data || data.scopes.length === 0) return null;
    const tripped = data.scopes.find((s) => s.tripped && s.enabled);
    if (tripped) return tripped;
    return (
      [...data.scopes]
        .filter((s) => s.enabled)
        .sort((a, b) => b.consecutive_losses - a.consecutive_losses)[0] ?? null
    );
  }, [data]);

  const doReset = async () => {
    if (!worst) return;
    setBusy(true);
    setErr(null);
    try {
      await circuitBreakerReset(worst.scope_key);
      await refresh();
      setConfirming(false);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  // Clean account (no scopes / zero losses) → stay invisible.
  if (!worst || (!worst.tripped && worst.consecutive_losses === 0)) return null;

  const tripped = worst.tripped;

  return (
    <>
      <div
        className="banner warn"
        role="alert"
        style={
          tripped
            ? {
                borderColor: "rgb(var(--down) / 0.45)",
                background: "rgb(var(--down) / 0.07)",
                marginBottom: 12,
              }
            : { marginBottom: 12 }
        }
      >
        <AlertTriangle
          size={16}
          style={{ flexShrink: 0, color: tripped ? "var(--red)" : "var(--amber)" }}
        />
        <span style={{ flex: 1 }}>
          {tripped ? (
            <>
              <strong style={{ color: "var(--red)" }}>Auto-exec halted</strong> —{" "}
              {worst.consecutive_losses}/{worst.threshold} consecutive losses tripped
              the circuit breaker. New auto-exec entries are blocked until you reset it.
            </>
          ) : (
            <>
              <strong>Circuit breaker armed</strong> — {worst.consecutive_losses}/
              {worst.threshold} consecutive losses. Auto-exec halts at{" "}
              {worst.threshold}.
            </>
          )}
          {err && !confirming && (
            <span style={{ color: "var(--red)", display: "block", fontSize: 12 }}>
              {err}
            </span>
          )}
        </span>
        {tripped && (
          <button
            className="btn danger solid sm"
            onClick={() => {
              setErr(null);
              setConfirming(true);
            }}
            title="Clear the tripped breaker and re-enable auto-execution"
          >
            Reset breaker
          </button>
        )}
      </div>

      <DangerConfirm
        open={confirming}
        title="Reset circuit breaker"
        phrase="RESET"
        busy={busy}
        error={err}
        onCancel={() => setConfirming(false)}
        onConfirm={doReset}
      >
        <p style={{ marginTop: 0 }}>
          Scope <span className="mono">{worst.scope_key}</span> tripped after{" "}
          <strong>{worst.consecutive_losses} consecutive losses</strong> (threshold{" "}
          {worst.threshold}). Resetting clears the trip and re-enables
          auto-execution immediately — make sure you understand why the losses
          happened first.
        </p>
      </DangerConfirm>
    </>
  );
}
