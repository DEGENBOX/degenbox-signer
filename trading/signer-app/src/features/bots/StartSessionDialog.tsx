// Start-session dialog — creates the gateway budget row
// (POST /api/trading/bot/sessions) against THIS client's wallet, then
// arms this device's engine (/bot/enable), exactly the two-step
// contract the old Bots page drove. Honesty note: the device engine
// signs with the PRIMARY executor wallet (local_daemon.rs bot_enable
// uses the :5829 slot), so arming a session created for a non-primary
// wallet is flagged before submit.

import { useEffect, useState } from "react";
import { Bot as BotIcon } from "lucide-react";
import { NumericField, shortAddr } from "@degenbox/ui";
import { Modal } from "../../components/ui";
import {
  LadderEditor,
  toLegSpecs,
  validateEditableLadder,
  type EditableLeg,
} from "../../components/LadderEditor";
import { ipc, LAMPORTS, type ClientInfo, type PresetLite } from "./ipc";

interface Props {
  /** The client whose wallet funds the session — null closes. */
  client: ClientInfo | null;
  /** Primary Sol executor address (status.sol_pubkey) for the
   * arm-honesty warning. */
  primaryAddress: string | null;
  onClose: () => void;
  /** Server row created; `armErr` set when device arming failed. */
  onStarted: (presetName: string, armErr: string | null) => void;
}

export function StartSessionDialog({ client, primaryAddress, onClose, onStarted }: Props) {
  const open = client !== null;
  const [presets, setPresets] = useState<PresetLite[] | null>(null);
  const [presetErr, setPresetErr] = useState<string | null>(null);
  const [presetId, setPresetId] = useState("");
  const [budget, setBudget] = useState("0.5");
  const [perTrade, setPerTrade] = useState("0.05");
  const [perToken, setPerToken] = useState("");
  const [hours, setHours] = useState("24");
  const [ladder, setLadder] = useState<EditableLeg[]>([]);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!open) return;
    setErr(null);
    setBusy(false);
    setPresetId("");
    setLadder([]);
    ipc
      .alphaPresets()
      .then((p) => {
        setPresets(p);
        setPresetErr(null);
      })
      .catch((e) => setPresetErr(String(e)));
  }, [open]);

  if (!client) return null;
  const walletPubkey = client.address;
  const notPrimary = primaryAddress != null && walletPubkey !== primaryAddress;

  const submit = async () => {
    setErr(null);
    try {
      if (!walletPubkey) {
        throw new Error("this client has no wallet address");
      }
      if (!presetId) {
        throw new Error(
          "pick a preset: the engine subscribes its signal stream; without one the session can never trade",
        );
      }
      const budgetSol = Number(budget);
      const perTradeSol = Number(perTrade);
      const hoursN = Math.max(1, Math.min(720, Math.round(Number(hours) || 0)));
      if (!Number.isFinite(budgetSol) || budgetSol <= 0) {
        throw new Error("budget must be > 0 SOL");
      }
      if (!Number.isFinite(perTradeSol) || perTradeSol <= 0) {
        throw new Error("per-trade size must be > 0 SOL");
      }
      if (perTradeSol > budgetSol) {
        throw new Error("per-trade size can't exceed the budget");
      }
      const perTokenSol = perToken.trim() ? Number(perToken) : null;
      if (perTokenSol != null && (!Number.isFinite(perTokenSol) || perTokenSol <= 0)) {
        throw new Error("per-token cap must be > 0 SOL (or empty)");
      }
      if (ladder.length > 0) {
        const lErr = validateEditableLadder(ladder);
        if (lErr) throw new Error(`ladder: ${lErr}`);
      }
      setBusy(true);
      const row = await ipc.botSessionCreate({
        preset_id: presetId,
        wallet_pubkey: walletPubkey,
        budget_lamports: Math.floor(budgetSol * LAMPORTS),
        per_trade_lamports: Math.floor(perTradeSol * LAMPORTS),
        ...(perTokenSol != null
          ? { per_token_cap_lamports: Math.floor(perTokenSol * LAMPORTS) }
          : {}),
        expires_at_unix_ms: Date.now() + hoursN * 3_600_000,
        ...(ladder.length > 0 ? { default_ladder: toLegSpecs(ladder) } : {}),
      });
      const name = presets?.find((p) => p.id === presetId)?.name ?? "preset";
      // Server row exists — now arm THIS device. Report truthfully.
      try {
        await ipc.botArm({
          session_id: row.id,
          preset_id: presetId,
          per_trade_lamports: Math.floor(perTradeSol * LAMPORTS),
          budget_lamports: Math.floor(budgetSol * LAMPORTS),
          spent_lamports: 0,
          per_token_cap_lamports:
            perTokenSol != null ? Math.floor(perTokenSol * LAMPORTS) : null,
        });
        onStarted(name, null);
      } catch (e) {
        onStarted(name, String(e));
      }
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  };

  const maxTrades =
    Number(budget) > 0 && Number(perTrade) > 0
      ? Math.floor(Number(budget) / Number(perTrade))
      : null;

  return (
    <Modal
      open={open}
      onClose={() => (busy ? undefined : onClose())}
      title={
        <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
          <BotIcon size={14} /> Start session · {client.label ?? shortAddr(walletPubkey, 5, 5)}
        </span>
      }
      width={520}
      locked={busy}
    >
      <div style={{ display: "grid", gap: 14 }}>
        <div className="field-group">
          <label className="field">Signal preset (required)</label>
          {presetErr ? (
            <div className="error-box">{presetErr}</div>
          ) : (
            <select
              className="input"
              value={presetId}
              onChange={(e) => setPresetId(e.target.value)}
            >
              <option value="">
                {presets === null
                  ? "loading presets…"
                  : presets.length === 0
                    ? "no presets (create one in the web app's scanner first)"
                    : "select a preset…"}
              </option>
              {(presets ?? []).map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          )}
          <div style={{ fontSize: 11, color: "var(--fg-faint)", marginTop: 4 }}>
            the engine subscribes this preset's live signal stream and auto-buys matches
          </div>
        </div>

        <div className="field-group">
          <label className="field">Budget</label>
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(110px, 1fr))",
              gap: 10,
            }}
          >
            <NumericField
              label="Budget"
              unit="SOL"
              value={budget}
              onChange={(t) => setBudget(t)}
              min={0}
              placeholder="0.5"
              disabled={busy}
            />
            <NumericField
              label="Trade"
              unit="SOL"
              value={perTrade}
              onChange={(t) => setPerTrade(t)}
              min={0}
              placeholder="0.05"
              disabled={busy}
            />
            <NumericField
              label="Token cap"
              unit="SOL"
              value={perToken}
              onChange={(t) => setPerToken(t)}
              min={0}
              placeholder="uncapped"
              clearable
              disabled={busy}
            />
            <NumericField
              label="Duration"
              unit="h"
              value={hours}
              onChange={(t) => setHours(t)}
              min={1}
              max={720}
              integer
              placeholder="24"
              disabled={busy}
            />
          </div>
          <div style={{ fontSize: 11, color: "var(--fg-faint)", marginTop: 6 }}>
            wallet: <span className="mono">{shortAddr(walletPubkey, 6, 6)}</span> · max
            trades: {maxTrades ?? "—"}
          </div>
        </div>

        {notPrimary && (
          <div className="banner warn" role="alert" style={{ margin: 0 }}>
            <span style={{ flex: 1 }}>
              This client is not the primary executor. This device's bot engine signs
              with the <strong>primary</strong> wallet. Make this client primary first if
              the session should spend from{" "}
              <span className="mono">{shortAddr(walletPubkey, 5, 5)}</span>.
            </span>
          </div>
        )}

        <div className="field-group">
          <label className="field">Auto TP/SL ladder on every bot buy (optional)</label>
          <LadderEditor value={ladder} onChange={setLadder} disabled={busy} />
        </div>

        {err && <div className="error-box">{err}</div>}

        <div className="modal-foot" style={{ marginTop: 0 }}>
          <button className="btn" disabled={busy} onClick={onClose}>
            Cancel
          </button>
          <button className="btn primary" disabled={busy} onClick={submit}>
            {busy ? "Starting…" : "Start + arm this device"}
          </button>
        </div>
      </div>
    </Modal>
  );
}
