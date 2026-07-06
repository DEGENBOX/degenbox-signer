// Per-client server-side budget caps (gateway `trading_clients` row):
// session budget + per-trade size, SOL-denominated, stored as
// lamports. Empty field = uncapped (the PATCH carries the matching
// clear flag). NumericField is the kit's string-owned numeric input.

import { useEffect, useState } from "react";
import { NumericField, lamportsFromSolText } from "@degenbox/ui";
import { Modal } from "../../components/ui";
import { ipc, LAMPORTS, type ClientInfo } from "./ipc";

interface Props {
  /** Client whose budget is being edited — null closes the dialog.
   * Must carry a gateway row (the button only renders when it does). */
  client: ClientInfo | null;
  onClose: () => void;
  onSaved: () => void;
}

export function BudgetDialog({ client, onClose, onSaved }: Props) {
  const [session, setSession] = useState("");
  const [perTrade, setPerTrade] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (!client) return;
    const b = client.gateway?.budget;
    setSession(
      b?.session_budget_lamports != null ? String(b.session_budget_lamports / LAMPORTS) : "",
    );
    setPerTrade(b?.per_trade_lamports != null ? String(b.per_trade_lamports / LAMPORTS) : "");
    setErr(null);
    setBusy(false);
  }, [client]);

  if (!client?.gateway) return null;
  const gwId = client.gateway.id;

  const save = async () => {
    setErr(null);
    try {
      const sessionLamports = lamportsFromSolText(session);
      const perTradeLamports = lamportsFromSolText(perTrade);
      if (session.trim() && (sessionLamports == null || sessionLamports <= 0)) {
        throw new Error("session budget must be > 0 SOL (or empty = uncapped)");
      }
      if (perTrade.trim() && (perTradeLamports == null || perTradeLamports <= 0)) {
        throw new Error("per-trade cap must be > 0 SOL (or empty = uncapped)");
      }
      if (
        sessionLamports != null &&
        perTradeLamports != null &&
        perTradeLamports > sessionLamports
      ) {
        throw new Error("per-trade cap can't exceed the session budget");
      }
      setBusy(true);
      await ipc.clientBudgetSet(gwId, {
        ...(sessionLamports != null
          ? { session_budget_lamports: sessionLamports }
          : { clear_session_budget_lamports: true }),
        ...(perTradeLamports != null
          ? { per_trade_lamports: perTradeLamports }
          : { clear_per_trade_lamports: true }),
      });
      onSaved();
      onClose();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open
      onClose={() => (busy ? undefined : onClose())}
      title={`Budget · ${client.label ?? "client"}`}
      width={420}
      locked={busy}
    >
      <p style={{ marginTop: 0 }}>
        Server-side caps for everything this client executes. Empty ={" "}
        <span className="mono">uncapped</span>.
      </p>
      <div style={{ display: "grid", gap: 12 }}>
        <NumericField
          label="Session"
          unit="SOL"
          value={session}
          onChange={(t) => setSession(t)}
          min={0}
          placeholder="uncapped"
          clearable
          disabled={busy}
          hint="hard stop across the whole session"
        />
        <NumericField
          label="Per-trade"
          unit="SOL"
          value={perTrade}
          onChange={(t) => setPerTrade(t)}
          min={0}
          placeholder="uncapped"
          clearable
          disabled={busy}
          hint="max size of any single buy"
        />
        {err && <div className="error-box">{err}</div>}
        <div className="modal-foot" style={{ marginTop: 0 }}>
          <button className="btn" disabled={busy} onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy} onClick={save}>
            {busy ? "Saving…" : "Save budget"}
          </button>
        </div>
      </div>
    </Modal>
  );
}
