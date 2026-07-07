// Spot↔Perp USDC transfer dialog (HL `usdClassTransfer`).
//
// HL keeps SPOT and PERP balances in SEPARATE wallets. Perps trade off the
// PERP balance only, so idle spot USDC has to be moved spot→perp before it
// can margin a position. This dialog lets the operator move funds without
// leaving for hyperliquid.xyz.
//
// The transfer is enqueued via the gateway (same money-path as orders):
// THIS daemon then signs the `usdClassTransfer` action and POSTs to HL. The
// gateway rejects fail-closed if the amount exceeds the source balance.

import { useState } from "react";
import { ArrowRightLeft } from "lucide-react";
import { commands } from "../../lib/commands";
import { Modal } from "../../components/ui";
import { fmtUsdOrDash } from "@degenbox/ui";

/** Floor to 6dp so "move all" never exceeds the source by float noise. */
function floor6(n: number): number {
  return Math.floor(n * 1_000_000) / 1_000_000;
}

export function SpotPerpTransferDialog({
  spotUsdc,
  perpUsd,
  paper,
  clientId,
  initialToPerp = true,
  onClose,
  onDone,
}: {
  /** Live SPOT USDC (source for spot→perp). `null` = unknown. */
  spotUsdc: number | null;
  /** Live PERP equity (source display for perp→spot). `null` = unknown. */
  perpUsd: number | null;
  paper: boolean;
  /** Vault client id whose config/token to use (defaults primary). */
  clientId?: string;
  initialToPerp?: boolean;
  onClose: () => void;
  onDone: (notice: string) => void;
}) {
  const [toPerp, setToPerp] = useState(initialToPerp);
  const [amount, setAmount] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const sourceLabel = toPerp ? "Spot" : "Perp";
  const destLabel = toPerp ? "Perp" : "Spot";
  const sourceBal = toPerp ? spotUsdc : perpUsd;

  const onMoveAll = () => {
    if (sourceBal == null || sourceBal <= 0) return;
    setAmount(String(floor6(sourceBal)));
  };

  const run = async () => {
    setErr(null);
    const n = Number(amount);
    if (!Number.isFinite(n) || n <= 0) {
      setErr("Enter an amount greater than 0.");
      return;
    }
    if (sourceBal != null && n > sourceBal + 1e-6) {
      setErr(
        `Amount ${fmtUsdOrDash(n)} exceeds your ${sourceLabel.toLowerCase()} balance ${fmtUsdOrDash(sourceBal)}.`,
      );
      return;
    }
    setBusy(true);
    try {
      const res = await commands.perps.transferSpotPerp(toPerp, amount, clientId);
      onDone(
        res.status === "paper"
          ? `Paper mode: ${sourceLabel}→${destLabel} transfer of $${amount} recorded (no live move).`
          : `Transfer queued for your signer: $${amount} ${sourceLabel.toLowerCase()} → ${destLabel.toLowerCase()} (${res.cloid}).`,
      );
      onClose();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open
      onClose={() => (busy ? undefined : onClose())}
      title="Transfer spot ↔ perp"
      width={420}
      locked={busy}
    >
      <p style={{ marginTop: 0, fontSize: 12, opacity: 0.8 }}>
        Move USDC between your Hyperliquid spot and perp wallets. Perps trade
        off the perp balance — % -of-equity sizing needs perp funded.
        {paper && (
          <span className="badge warn" style={{ marginLeft: 6 }}>
            paper
          </span>
        )}
      </p>

      <div style={{ display: "flex", gap: 10, marginBottom: 12 }}>
        <div className="field-group" style={{ flex: 1 }}>
          <label className="field">Spot USDC</label>
          <div className="mono">{fmtUsdOrDash(spotUsdc)}</div>
        </div>
        <div className="field-group" style={{ flex: 1 }}>
          <label className="field">Perp equity</label>
          <div className="mono">{fmtUsdOrDash(perpUsd)}</div>
        </div>
      </div>

      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 8,
          marginBottom: 12,
        }}
      >
        <span className="mono">{sourceLabel}</span>
        <button
          type="button"
          className="btn sm"
          onClick={() => {
            setToPerp((v) => !v);
            setAmount("");
            setErr(null);
          }}
          title="Swap direction"
        >
          <ArrowRightLeft size={13} />
        </button>
        <span className="mono">{destLabel}</span>
      </div>

      <div className="field-group">
        <div
          style={{
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
          }}
        >
          <label className="field">Amount (USD)</label>
          <button
            type="button"
            className="btn xs"
            onClick={onMoveAll}
            disabled={sourceBal == null || sourceBal <= 0}
          >
            Move all {sourceLabel.toLowerCase()}
          </button>
        </div>
        <input
          type="text"
          inputMode="decimal"
          value={amount}
          onChange={(e) => setAmount(e.target.value.replace(/[^0-9.]/g, ""))}
          placeholder="0.00"
          autoFocus
        />
        <p style={{ fontSize: 11, opacity: 0.7, marginTop: 4 }}>
          Signed locally by your desktop signer · min $1.
        </p>
      </div>

      {err && (
        <div className="error-box" style={{ marginTop: 8 }}>
          {err}
        </div>
      )}

      <div
        style={{
          display: "flex",
          justifyContent: "flex-end",
          gap: 8,
          marginTop: 14,
        }}
      >
        <button type="button" className="btn" onClick={onClose} disabled={busy}>
          Cancel
        </button>
        <button
          type="button"
          className="btn primary"
          onClick={run}
          disabled={busy || !amount}
        >
          {busy ? "Submitting…" : `Move to ${destLabel.toLowerCase()}`}
        </button>
      </div>
    </Modal>
  );
}
