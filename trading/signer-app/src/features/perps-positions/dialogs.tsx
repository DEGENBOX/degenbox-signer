// Close / TP-SL dialogs for Perpetuals positions — carried over from
// the retired pages/HlPositions.tsx (W4.1 replaces that page; the
// dialogs' execution semantics are untouched). Both routes resolve the
// LIVE position size signer-side, so a stale mark can't mis-size them:
//   · ClosePositionDialog → ipc.hlClosePosition (reduce-only market
//     exit, type-to-confirm gated, percent presets + custom);
//   · TpslDialog → ipc.hlPlaceTpsl (reduce-only triggers with soft
//     direction sanity vs entry — the signer re-validates vs mark).
// New vs the old page: `initialPercent` lets the table's quick
// 25/50/100 buttons pre-fill the close percent (Sol quick-sell mirror).

import { useEffect, useState } from "react";
import { ipc, type HlPosition } from "../../ipc";
import { Modal, Pnl } from "../../components/ui";
import { getSkipCloseConfirm, setSkipCloseConfirm } from "../../lib/prefs";

// ─── Close / reduce dialog (type-to-confirm) ────────────────────────

export function ClosePositionDialog({
  position,
  paper,
  initialPercent = 100,
  onClose,
  onDone,
}: {
  position: HlPosition | null;
  paper: boolean;
  /** Pre-filled close percent (quick 25/50/100 buttons). */
  initialPercent?: number;
  onClose: () => void;
  onDone: (notice: string) => void;
}) {
  const [percent, setPercent] = useState("100");
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  // "Don't ask again" — governs BOTH venues' close flows via localStorage.
  // Persisted the moment it's toggled (so cancelling still keeps the choice).
  const [skip, setSkip] = useState(() => getSkipCloseConfirm());

  useEffect(() => {
    if (position) {
      setPercent(String(initialPercent));
      setTyped("");
      setErr(null);
      setBusy(false);
      setSkip(getSkipCloseConfirm());
    }
  }, [position, initialPercent]);

  if (!position) return null;
  const coin = position.coin;
  const pctNum = Number(percent);
  const pctValid = Number.isFinite(pctNum) && pctNum > 0 && pctNum <= 100;
  // When skip is set the type-to-confirm gate is bypassed.
  const gateOk = skip || typed === coin;

  const run = async () => {
    if (!pctValid || !gateOk) return;
    setBusy(true);
    setErr(null);
    try {
      const res = await ipc.hlClosePosition(coin, pctNum);
      onDone(
        res.status === "paper"
          ? `Paper mode: ${pctNum}% close of ${coin} recorded (no live order).`
          : `Close queued for your signer: ${pctNum}% of ${coin} (${res.cloid}).`,
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
      title={`Close ${coin} (${position.side})`}
      width={420}
      locked={busy}
    >
      <p style={{ marginTop: 0 }}>
        Reduce-only market exit, sized against the <strong>live</strong> position at
        execution time. Size {position.szi} · uPnL{" "}
        <Pnl value={position.unrealized_pnl} />
        {paper && (
          <span className="badge warn" style={{ marginLeft: 6 }}>
            paper
          </span>
        )}
      </p>
      <div className="field-group">
        <label className="field">Percent of position to close</label>
        <div style={{ display: "flex", gap: 6, alignItems: "center", flexWrap: "wrap" }}>
          {["25", "50", "75", "100"].map((v) => (
            <button
              key={v}
              type="button"
              className={`btn sm ${percent === v ? "primary" : ""}`}
              disabled={busy}
              onClick={() => setPercent(v)}
            >
              {v}%
            </button>
          ))}
          <input
            className="input mono"
            style={{ width: 80 }}
            inputMode="decimal"
            value={percent}
            disabled={busy}
            onChange={(e) => setPercent(e.target.value)}
            aria-label="Custom close percent"
          />
          <span className="mono" style={{ fontSize: 11, color: "var(--fg-faint)" }}>
            %
          </span>
        </div>
        {!pctValid && <div className="error-box">percent must be in (0, 100]</div>}
      </div>
      <div className="field-group" style={{ marginTop: 12 }}>
        <label className="field">
          Type <span className="mono">{coin}</span> to confirm
        </label>
        <input
          className="input mono"
          value={typed}
          placeholder={coin}
          disabled={busy || skip}
          onChange={(e) => setTyped(e.target.value)}
          autoFocus
        />
      </div>
      <label
        style={{
          display: "flex",
          alignItems: "center",
          gap: 8,
          marginTop: 10,
          fontSize: 12,
          color: "var(--fg-faint)",
          cursor: busy ? "default" : "pointer",
        }}
        title="Skip this confirmation for every position close (Perps + Solana). Toggle back off here next time it shows, or in the Application settings card."
      >
        <input
          type="checkbox"
          checked={skip}
          disabled={busy}
          onChange={(e) => {
            const next = e.target.checked;
            setSkip(next);
            setSkipCloseConfirm(next); // persist immediately
          }}
        />
        Don't ask again — close positions without confirming
      </label>
      {err && <div className="error-box">{err}</div>}
      <div className="modal-foot">
        <button className="btn" disabled={busy} onClick={onClose}>
          Cancel
        </button>
        <button
          className="btn danger solid"
          disabled={busy || !pctValid || !gateOk}
          onClick={run}
        >
          {busy ? "Working…" : `Close ${pctValid ? pctNum : "…"}%`}
        </button>
      </div>
    </Modal>
  );
}

// ─── TP/SL dialog ───────────────────────────────────────────────────

export function TpslDialog({
  position,
  paper,
  onClose,
  onDone,
}: {
  position: HlPosition | null;
  paper: boolean;
  onClose: () => void;
  onDone: (notice: string) => void;
}) {
  const [tp, setTp] = useState("");
  const [sl, setSl] = useState("");
  const [closePct, setClosePct] = useState("100");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    if (position) {
      setTp("");
      setSl("");
      setClosePct("100");
      setErr(null);
      setBusy(false);
    }
  }, [position]);

  if (!position) return null;
  const coin = position.coin;
  const isLong = position.side === "long";
  const entry = Number(position.entry_px);
  const pctNum = Number(closePct);
  const pctValid = Number.isFinite(pctNum) && pctNum > 0 && pctNum <= 100;
  const tpNum = tp.trim() === "" ? null : Number(tp);
  const slNum = sl.trim() === "" ? null : Number(sl);
  const anySet = tpNum != null || slNum != null;
  const numsValid =
    (tpNum == null || (Number.isFinite(tpNum) && tpNum > 0)) &&
    (slNum == null || (Number.isFinite(slNum) && slNum > 0));
  // Soft direction sanity vs entry (the signer re-validates vs mark).
  const tpWarn =
    tpNum != null && Number.isFinite(entry) && entry > 0
      ? isLong
        ? tpNum <= entry
        : tpNum >= entry
      : false;
  const slWarn =
    slNum != null && Number.isFinite(entry) && entry > 0
      ? isLong
        ? slNum >= entry
        : slNum <= entry
      : false;

  const run = async () => {
    if (!anySet || !numsValid || !pctValid) return;
    setBusy(true);
    setErr(null);
    try {
      const res = await ipc.hlPlaceTpsl(
        coin,
        tpNum != null ? tp.trim() : null,
        slNum != null ? sl.trim() : null,
        pctNum,
      );
      onDone(
        res.status === "paper"
          ? `Paper mode: TP/SL for ${coin} recorded (no live trigger).`
          : `TP/SL queued for your signer: ${res.cloids.length} trigger${
              res.cloids.length === 1 ? "" : "s"
            } on ${coin}.`,
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
      title={`TP/SL · ${coin} (${position.side})`}
      width={520}
      locked={busy}
    >
      <p style={{ marginTop: 0 }}>
        Reduce-only triggers attached to the live position. Entry{" "}
        <span className="mono">{position.entry_px ?? "—"}</span> · size{" "}
        <span className="mono">{position.szi}</span>
        {paper && (
          <span className="badge warn" style={{ marginLeft: 6 }}>
            paper
          </span>
        )}
      </p>
      <div style={{ display: "flex", gap: 10, flexWrap: "wrap" }}>
        <label style={{ display: "grid", gap: 3, fontSize: 11, color: "var(--fg-faint)" }}>
          Take-profit price (USD)
          <input
            className="input mono"
            style={{ width: 140 }}
            inputMode="decimal"
            value={tp}
            placeholder="optional"
            disabled={busy}
            onChange={(e) => setTp(e.target.value)}
          />
        </label>
        <label style={{ display: "grid", gap: 3, fontSize: 11, color: "var(--fg-faint)" }}>
          Stop-loss price (USD)
          <input
            className="input mono"
            style={{ width: 140 }}
            inputMode="decimal"
            value={sl}
            placeholder="optional"
            disabled={busy}
            onChange={(e) => setSl(e.target.value)}
          />
        </label>
        <label style={{ display: "grid", gap: 3, fontSize: 11, color: "var(--fg-faint)" }}>
          Close % when hit
          <input
            className="input mono"
            style={{ width: 90 }}
            inputMode="decimal"
            value={closePct}
            disabled={busy}
            onChange={(e) => setClosePct(e.target.value)}
          />
        </label>
      </div>
      {tpWarn && (
        <div className="error-box">
          TP {tp} is on the wrong side of entry for a {position.side}. It would fire
          immediately.
        </div>
      )}
      {slWarn && (
        <div className="error-box">
          SL {sl} is on the wrong side of entry for a {position.side}. It would fire
          immediately.
        </div>
      )}
      {!pctValid && <div className="error-box">close % must be in (0, 100]</div>}
      {err && <div className="error-box">{err}</div>}
      <div className="modal-foot">
        <button className="btn" disabled={busy} onClick={onClose}>
          Cancel
        </button>
        <button
          className="btn primary"
          disabled={busy || !anySet || !numsValid || !pctValid || tpWarn || slWarn}
          onClick={run}
        >
          {busy ? "Working…" : "Set triggers"}
        </button>
      </div>
    </Modal>
  );
}
