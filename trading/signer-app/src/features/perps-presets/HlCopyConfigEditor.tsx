// Perps copy-config editor — iteration-3 rebuild: an INLINE expanding
// editor (no modal — platform rule) with the platform form language
// (labels above full-width controls, grouped sections, mono numbers).
// Same wire shapes + validation + typed Tauri commands as before; only
// the chrome + input primitives changed. Rendered inside
// HlCopyTradeSection; delete keeps a destructive type-to-confirm.

import { useEffect, useState } from "react";
import { Save, Trash2 } from "lucide-react";
import { shortAddr } from "@degenbox/ui";
import { DangerConfirm, Segmented } from "../../components/ui";
import { CheckField, Field, FormGroup, InlineEditor, NumField, TextField } from "../../components/form";
import { commands } from "../../lib/commands";
import {
  deleteHlConfig,
  friendlyGatewayError,
  isHlAddress,
  type HlCopyConfigFull,
  type HlCopyConfigPatch,
} from "./ipc";

const DEFAULT_TP_LEVELS: Array<{ mult: number; close_pct: number }> = [
  { mult: 1.5, close_pct: 33 },
  { mult: 2, close_pct: 33 },
  { mult: 5, close_pct: 34 },
];

interface Props {
  onClose: () => void;
  /** Saved / deleted — the owner reloads its list. */
  onSaved: () => void;
  /** Existing config (edit) or null (create — leader field shows). */
  existing: HlCopyConfigFull | null;
}

export function HlCopyConfigEditor({ onClose, onSaved, existing }: Props) {
  const [leaderInput, setLeaderInput] = useState("");
  const [enabled, setEnabled] = useState(true);
  // Modes shown: 3 = fixed $ per copy, 0 = % of leader's size,
  // 2 = balance-matched. Legacy mode 1 ("same as leader") is gone from
  // the UI — it equals mode 0 at 100% — and loads as exactly that; the
  // stored row is only rewritten when the user saves.
  const [followMode, setFollowMode] = useState<"0" | "2" | "3">("0");
  const [fixedSizeUsd, setFixedSizeUsd] = useState("");
  const [scalePct, setScalePct] = useState("100");
  const [equityBasis, setEquityBasis] = useState<"0" | "1" | "2">("0");
  const [maxPositionUsd, setMaxPositionUsd] = useState("");
  const [minFillUsd, setMinFillUsd] = useState("");
  const [leverageCap, setLeverageCap] = useState("");
  const [allowlist, setAllowlist] = useState("");
  const [drawdownPct, setDrawdownPct] = useState("0");
  const [slippagePct, setSlippagePct] = useState("2");
  const [mirrorCloses, setMirrorCloses] = useState(true);
  const [slStrategy, setSlStrategy] = useState<"0" | "1" | "2" | "3">("0");
  const [slPct, setSlPct] = useState("10");
  const [tpStrategy, setTpStrategy] = useState<"0" | "1" | "2">("0");
  const [tpLevels, setTpLevels] = useState(DEFAULT_TP_LEVELS);
  const [retryPolicy, setRetryPolicy] = useState<"0" | "1" | "2">("0");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [deleteErr, setDeleteErr] = useState<string | null>(null);

  // Seed the form from the target config on mount / when it changes.
  useEffect(() => {
    setErr(null);
    setBusy(false);
    setConfirmDelete(false);
    setDeleteErr(null);
    if (existing) {
      setLeaderInput(existing.target_wallet);
      setEnabled(existing.enabled);
      const storedMode = (existing.follow_mode ?? 0) | 0;
      if (storedMode === 1) {
        // Legacy "same as leader" = "% of leader's size" at 100.
        setFollowMode("0");
        setScalePct("100");
      } else {
        setFollowMode(storedMode === 2 || storedMode === 3 ? (String(storedMode) as "2" | "3") : "0");
        setScalePct(String(Math.round(Number(existing.scale_factor) * 100)));
      }
      setFixedSizeUsd(existing.fixed_size_usd ?? "");
      setEquityBasis(String((existing.equity_basis ?? 0) | 0) as "0" | "1" | "2");
      setMaxPositionUsd(existing.max_position_usd ?? "");
      setMinFillUsd(existing.min_fill_usd ?? "");
      setLeverageCap(existing.leverage_cap != null ? String(existing.leverage_cap) : "");
      setAllowlist((existing.coin_allowlist ?? []).join(", "));
      setDrawdownPct(String(existing.drawdown_stop_pct ?? 0));
      setSlippagePct(String((existing.slippage_limit_bps ?? 200) / 100));
      setMirrorCloses(existing.mirror_closes);
      setSlStrategy(String((existing.sl_placement_strategy ?? 0) | 0) as "0" | "1" | "2" | "3");
      setSlPct(existing.sl_placement_pct != null ? String(existing.sl_placement_pct) : "10");
      setTpStrategy(String((existing.tp_placement_strategy ?? 0) | 0) as "0" | "1" | "2");
      setTpLevels(existing.tp_levels_json ?? DEFAULT_TP_LEVELS);
      setRetryPolicy(String((existing.retry_on_reject ?? 0) | 0) as "0" | "1" | "2");
    } else {
      setLeaderInput("");
      setEnabled(true);
      setFollowMode("0");
      setFixedSizeUsd("");
      setScalePct("100");
      setEquityBasis("0");
      setMaxPositionUsd("1000");
      setMinFillUsd("50");
      setLeverageCap("");
      setAllowlist("");
      setDrawdownPct("0");
      setSlippagePct("2");
      setMirrorCloses(true);
      setSlStrategy("0");
      setSlPct("10");
      setTpStrategy("0");
      setTpLevels(DEFAULT_TP_LEVELS);
      setRetryPolicy("0");
    }
  }, [existing]);

  const leaderValid = existing != null || isHlAddress(leaderInput);

  const save = async () => {
    setErr(null);
    try {
      const fixedMode = followMode === "3";
      let fixedN: number | null = null;
      if (fixedMode) {
        fixedN = Number(fixedSizeUsd);
        if (!Number.isFinite(fixedN) || fixedN <= 0) {
          throw new Error("fixed size must be a dollar amount above 0");
        }
      }
      const scalePctN = fixedMode ? 100 : Number(scalePct);
      if (!Number.isFinite(scalePctN) || scalePctN <= 0 || scalePctN > 1000) {
        throw new Error("size % must be between 1 and 1000 (100 mirrors the leader)");
      }
      const scaleN = scalePctN / 100;
      const slip = Number(slippagePct);
      if (!Number.isFinite(slip) || slip < 0 || slip > 100) {
        throw new Error("slippage must be 0..100%");
      }
      const dd = Math.round(Number(drawdownPct) || 0);
      if (dd < 0 || dd > 50) throw new Error("drawdown stop must be 0..50%");
      const lev = leverageCap.trim()
        ? Math.max(1, Math.min(125, Math.round(Number(leverageCap))))
        : null;
      const slPctN =
        slStrategy === "2" || slStrategy === "3"
          ? Math.max(1, Math.min(100, Math.round(Number(slPct) || 0)))
          : null;
      const levels =
        tpStrategy === "2"
          ? tpLevels
              .filter(
                (l) =>
                  Number.isFinite(l.mult) &&
                  l.mult > 0 &&
                  Number.isFinite(l.close_pct) &&
                  l.close_pct > 0,
              )
              .map((l) => ({
                mult: l.mult,
                close_pct: Math.min(100, Math.max(1, Math.round(l.close_pct))),
              }))
          : null;
      if (tpStrategy === "2" && (levels?.length ?? 0) === 0) {
        throw new Error("fixed-level TP needs at least one level");
      }
      const patch: HlCopyConfigPatch = {
        enabled,
        follow_mode: Number(followMode),
        // Only meaningful in fixed mode; omitted otherwise so an
        // existing stored value is kept (PATCH coalesce semantics).
        ...(fixedMode && fixedN != null
          ? { fixed_size_usd: String(fixedN) }
          : {}),
        scale_factor: scaleN.toFixed(2),
        equity_basis: Number(equityBasis),
        max_position_usd: maxPositionUsd.trim() === "" ? null : maxPositionUsd.trim(),
        min_fill_usd: minFillUsd.trim() === "" ? null : minFillUsd.trim(),
        leverage_cap: lev,
        coin_allowlist: parseAllowlist(allowlist),
        drawdown_stop_pct: dd > 0 ? dd : null,
        slippage_limit_bps: Math.max(0, Math.min(10000, Math.round(slip * 100))),
        mirror_closes: mirrorCloses,
        sl_placement_strategy: Number(slStrategy),
        sl_placement_pct: slPctN,
        tp_placement_strategy: Number(tpStrategy),
        tp_levels_json: levels,
        retry_on_reject: Number(retryPolicy),
      };
      setBusy(true);
      if (existing) {
        await commands.perps.copyConfigUpdate(existing.id, patch);
      } else {
        const addr = leaderInput.trim();
        if (!isHlAddress(addr)) throw new Error("paste a valid 0x… wallet address");
        await commands.perps.copyConfigCreate({ target_wallet: addr, ...patch });
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
      await deleteHlConfig(existing.id);
      setConfirmDelete(false);
      onSaved();
      onClose();
    } catch (e) {
      setDeleteErr(friendlyGatewayError(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <InlineEditor
        anchored
        columns={2}
        title={existing ? "Copy settings" : "Follow a Perpetuals wallet"}
        subtitle={
          existing ? (
            <span className="mono">{existing.target_wallet}</span>
          ) : (
            "Mirror another wallet's perp trades. One follow at a time per wallet; an active caller follow counts too."
          )
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
                <Trash2 size={13} /> Delete…
              </button>
            )}
            <button className="btn" disabled={busy} onClick={onClose}>
              Cancel
            </button>
            <button className="btn primary" disabled={busy || !leaderValid} onClick={save}>
              <Save size={13} /> {busy ? "Saving…" : existing ? "Save config" : "Start copying"}
            </button>
          </>
        }
      >
        <FormGroup title="Leader">
          {existing ? (
            <Field label="Leader wallet">
              <div className="mono field-static">{existing.target_wallet}</div>
            </Field>
          ) : (
            <TextField
              label="Leader wallet"
              value={leaderInput}
              onChange={setLeaderInput}
              mono
              autoFocus
              placeholder="0xabc… (the wallet whose perp trades you mirror)"
              hint={
                isHlAddress(leaderInput)
                  ? "valid address. Mirroring starts when you save"
                  : leaderInput.trim()
                    ? "not a valid 0x… address yet"
                    : "paste the leader's Hyperliquid wallet address"
              }
            />
          )}
          <CheckField
            label="Following (copies trades while on)"
            checked={enabled}
            onChange={setEnabled}
          />
        </FormGroup>

        <FormGroup title="Copy mode">
          <Field
            label="How each trade is sized"
            hint={
              followMode === "3"
                ? "every copy trades the same dollar amount, no matter how big the leader's trade is"
                : followMode === "0"
                  ? "a share of the leader's trade size, in percent. 100 mirrors it exactly"
                  : "match the share of equity the leader used, applied to your equity. Adapts as balances change"
            }
          >
            <Segmented
              options={[
                { value: "3", label: "Fixed $" },
                { value: "0", label: "% of leader's size" },
                { value: "2", label: "Balance-matched" },
              ]}
              value={followMode}
              onChange={setFollowMode}
            />
          </Field>
          {followMode === "3" && (
            <NumField
              label="Size per copy"
              unit="$"
              value={fixedSizeUsd}
              onChange={setFixedSizeUsd}
              placeholder="e.g. 100"
              hint="the dollar amount every mirrored trade uses"
            />
          )}
          {(followMode === "0" || followMode === "2") && (
            <NumField
              label="Size"
              unit="%"
              value={scalePct}
              onChange={setScalePct}
              inputMode="numeric"
              placeholder="100"
              hint="100 mirrors the leader, 50 halves it, 200 doubles it"
            />
          )}
          {followMode === "2" && (
            <Field label="Equity basis">
              <Segmented
                options={[
                  { value: "0", label: "total equity" },
                  { value: "1", label: "cash balance" },
                  { value: "2", label: "available" },
                ]}
                value={equityBasis}
                onChange={setEquityBasis}
              />
            </Field>
          )}
        </FormGroup>

        <FormGroup title="Sizing & leverage">
          <NumField
            label="Max position"
            unit="$"
            value={maxPositionUsd}
            onChange={setMaxPositionUsd}
            placeholder="unlimited"
          />
          <NumField
            label="Min fill"
            unit="$"
            value={minFillUsd}
            onChange={setMinFillUsd}
            placeholder="any"
            hint="skip leader fills below this"
          />
          <NumField
            label="Max leverage"
            unit="×"
            value={leverageCap}
            onChange={setLeverageCap}
            inputMode="numeric"
            placeholder="follow the leader"
            hint="copies above this leverage get clamped down to it"
          />
        </FormGroup>

        <FormGroup title="Filters & risk">
          <TextField
            label="Coin allowlist"
            value={allowlist}
            onChange={setAllowlist}
            mono
            placeholder="BTC, ETH, SOL… (empty = every coin)"
          />
          <NumField
            label="Drawdown stop"
            unit="%"
            value={drawdownPct}
            onChange={setDrawdownPct}
            inputMode="numeric"
            placeholder="0"
            hint="0 = off"
          />
          <NumField
            label="Max slippage"
            unit="%"
            value={slippagePct}
            onChange={setSlippagePct}
            placeholder="2"
          />
          <CheckField
            label="Sell when the leader sells"
            checked={mirrorCloses}
            onChange={setMirrorCloses}
            hint="follows every close or reduction of the leader's position, market and limit alike. We track their fills, so the order type never matters."
          />
        </FormGroup>

        <FormGroup title="Stop-loss">
          <Field label="Placement">
            <Segmented
              options={[
                { value: "0", label: "none" },
                { value: "1", label: "mirror leader" },
                { value: "2", label: "fixed %" },
                { value: "3", label: "trailing" },
              ]}
              value={slStrategy}
              onChange={setSlStrategy}
            />
          </Field>
          {(slStrategy === "2" || slStrategy === "3") && (
            <NumField label="SL distance" unit="%" value={slPct} onChange={setSlPct} inputMode="numeric" placeholder="10" />
          )}
        </FormGroup>

        <FormGroup title="Take-profit">
          <Field label="Placement">
            <Segmented
              options={[
                { value: "0", label: "none" },
                { value: "1", label: "mirror leader" },
                { value: "2", label: "fixed levels" },
              ]}
              value={tpStrategy}
              onChange={setTpStrategy}
            />
          </Field>
          {tpStrategy === "2" && (
            <div className="tp-levels">
              {tpLevels.map((row, idx) => (
                <div key={idx} className="tp-level-row">
                  <span className="mono tp-level-tag">TP{idx + 1}</span>
                  <input
                    className="input mono"
                    inputMode="decimal"
                    aria-label={`TP${idx + 1} multiple`}
                    value={String(row.mult)}
                    onChange={(e) =>
                      setTpLevels(
                        tpLevels.map((r, i) =>
                          i === idx
                            ? { ...r, mult: Number(e.target.value.replace(/[^0-9.]/g, "")) || 0 }
                            : r,
                        ),
                      )
                    }
                  />
                  <span className="mono tp-level-x">× close</span>
                  <input
                    className="input mono"
                    inputMode="numeric"
                    aria-label={`TP${idx + 1} close percent`}
                    value={String(row.close_pct)}
                    onChange={(e) =>
                      setTpLevels(
                        tpLevels.map((r, i) =>
                          i === idx
                            ? { ...r, close_pct: Number(e.target.value.replace(/[^0-9]/g, "")) || 0 }
                            : r,
                        ),
                      )
                    }
                  />
                  <span className="mono tp-level-x">%</span>
                  <button
                    type="button"
                    className="btn icon"
                    title="Remove level"
                    onClick={() => setTpLevels(tpLevels.filter((_, i) => i !== idx))}
                  >
                    ×
                  </button>
                </div>
              ))}
              <button
                type="button"
                className="btn xs"
                style={{ justifySelf: "start" }}
                onClick={() => {
                  const last = tpLevels[tpLevels.length - 1];
                  setTpLevels([...tpLevels, { mult: last ? last.mult + 1 : 1.5, close_pct: 33 }]);
                }}
              >
                + Add level
              </button>
            </div>
          )}
        </FormGroup>

        <FormGroup title="Retry on reject">
          <Field label="When a copy order is rejected">
            <Segmented
              options={[
                { value: "0", label: "drop" },
                { value: "1", label: "retry once" },
                { value: "2", label: "retry until fill" },
              ]}
              value={retryPolicy}
              onChange={setRetryPolicy}
            />
          </Field>
        </FormGroup>

        {err && <div className="error-box">{err}</div>}
      </InlineEditor>

      <DangerConfirm
        open={confirmDelete}
        title="Delete copy config"
        phrase="delete"
        busy={busy}
        error={deleteErr}
        onCancel={() => setConfirmDelete(false)}
        onConfirm={doDelete}
      >
        <p style={{ marginTop: 0 }}>
          Stop copying <strong>{existing ? shortAddr(existing.target_wallet, 6, 4) : ""}</strong>{" "}
          and delete its config. Past trades and open positions stay untouched.
        </p>
      </DangerConfirm>
    </>
  );
}

function parseAllowlist(text: string): string[] {
  return Array.from(
    new Set(
      text
        .split(/[\s,]+/)
        .map((s) => s.trim().toUpperCase())
        .filter(Boolean),
    ),
  );
}
