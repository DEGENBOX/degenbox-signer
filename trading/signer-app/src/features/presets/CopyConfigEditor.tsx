// Solana copy-config editor — v0.3.0 slice 9 (spec §F), full model.
//
//  * Two-column inline editor (never a modal), anchored under the row
//    that opened it.
//  * Sizing = all four modes (D7): fixed SOL (0), % of my balance (1),
//    % of the leader's buy (2, `buy_size_pct`), or match the leader's
//    conviction (3, `balance_pct` — the leader's cash fraction applied
//    to OUR cash, 4-asset basis D8).
//  * Per-config copy budget (D11) + double-buy protection (D10).
//  * "Risk cap" is GONE (decision D9). An existing legacy cap shows as
//    a removable read-only line; nothing writes the field any more.
//  * TP/SL = the SAME shared ladder editor as the preset editor, now in
//    its v2 dialect (sell-% of remaining + per-rung stop moves) — the
//    config routes accept the LadderSpec v2 object since 56e4a51.
//
// Same IPC + lockstep copy-feed fix + paste-to-follow as before.

import { useEffect, useMemo, useState } from "react";
import { Save, Trash2 } from "lucide-react";
import { shortAddr } from "@degenbox/ui";
import {
  LadderSpecEditor,
  draftFromLadderSpec,
  ladderDraftFromLegSpecs,
  ladderSpecFromDraft,
  validateLadderDraft,
  type LadderDraft,
} from "../../components/LadderSpecEditor";
import type { LegSpec } from "../../ipc";
import { DangerConfirm, Segmented } from "../../components/ui";
import {
  CheckField,
  Field,
  FormGroup,
  InlineEditor,
  NumField,
  TextField,
} from "../../components/form";
import { commands } from "../../lib/commands";
import {
  createTrackedWallet,
  ipc,
  isSolanaAddress,
  solText,
  type SolCopyConfigCreate,
  type SolCopyConfigFull,
  type SolCopyConfigPatch,
  type TrackedWallet,
} from "./ipc";

type SellMode = "copy" | "ladder" | "both";

/** UI sizing choice ↔ gateway `sizing_mode`. */
type SizingChoice = "fixed" | "pct" | "leaderSize" | "leaderCash";

const SIZING_TO_MODE: Record<SizingChoice, number> = {
  fixed: 0,
  pct: 1,
  leaderSize: 2,
  leaderCash: 3,
};

function sizingFromMode(mode: number): SizingChoice {
  switch (mode) {
    case 1:
      return "pct";
    case 2:
      return "leaderSize";
    case 3:
      return "leaderCash";
    default:
      return "fixed";
  }
}

/** Seed the ladder draft from whatever shape the gateway stored —
 *  LadderSpec v2 object first, legacy LegSpec[] as the fallback. */
function draftFromStored(raw: unknown): LadderDraft {
  return (
    draftFromLadderSpec(raw) ??
    ladderDraftFromLegSpecs(Array.isArray(raw) ? (raw as LegSpec[]) : null)
  );
}

const hasRungs = (d: LadderDraft) => d.baseSl !== null || d.rungs.length > 0;

interface Props {
  onClose: () => void;
  /** Saved / deleted — the owner reloads its list. */
  onSaved: () => void;
  /** Existing config (edit) or null (create — leader field shows). */
  existing: SolCopyConfigFull | null;
}

export function CopyConfigEditor({ onClose, onSaved, existing }: Props) {
  const [wallets, setWallets] = useState<TrackedWallet[] | null>(null);
  const [leaderInput, setLeaderInput] = useState("");
  const [enabled, setEnabled] = useState(true);
  const [sizingMode, setSizingMode] = useState<SizingChoice>("fixed");
  const [fixedSol, setFixedSol] = useState("0.1");
  const [pctOfBalance, setPctOfBalance] = useState("5");
  const [buySizePct, setBuySizePct] = useState("100");
  const [balancePct, setBalancePct] = useState("100");
  const [dropLegacyCap, setDropLegacyCap] = useState(false);
  const [budgetSol, setBudgetSol] = useState("");
  const [resetBudget, setResetBudget] = useState(false);
  const [singleBuy, setSingleBuy] = useState(false);
  const [minSourceUsd, setMinSourceUsd] = useState("");
  const [cooldownSecs, setCooldownSecs] = useState("0");
  const [slippagePct, setSlippagePct] = useState("2");
  const [sellMode, setSellMode] = useState<SellMode>("copy");
  const [ladder, setLadder] = useState<LadderDraft>(ladderDraftFromLegSpecs(null));
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
    setDropLegacyCap(false);
    setResetBudget(false);
    if (existing) {
      setLeaderInput(existing.leader);
      setEnabled(existing.enabled);
      setSizingMode(sizingFromMode(existing.sizing_mode));
      setFixedSol(existing.fixed_sol_lamports != null ? solText(existing.fixed_sol_lamports) : "0.1");
      setPctOfBalance(
        existing.pct_of_balance_bps != null ? String(existing.pct_of_balance_bps / 100) : "5",
      );
      setBuySizePct(existing.buy_size_pct != null ? String(existing.buy_size_pct) : "100");
      setBalancePct(existing.balance_pct != null ? String(existing.balance_pct) : "100");
      setBudgetSol(
        existing.copy_budget_lamports != null ? solText(existing.copy_budget_lamports) : "",
      );
      setSingleBuy(existing.single_buy_per_token);
      setMinSourceUsd(existing.min_source_buy_usd ?? "");
      setCooldownSecs(String(existing.per_mint_cooldown_secs));
      setSlippagePct(String(existing.slippage_bps / 100));
      const draft = draftFromStored(existing.default_ladder);
      const hasLadder = hasRungs(draft);
      setSellMode(
        existing.mirror_sells ? (hasLadder ? "both" : "copy") : hasLadder ? "ladder" : "copy",
      );
      setLadder(draft);
    } else {
      setLeaderInput("");
      setEnabled(true);
      setSizingMode("fixed");
      setFixedSol("0.1");
      setPctOfBalance("5");
      setBuySizePct("100");
      setBalancePct("100");
      setBudgetSol("");
      setSingleBuy(false);
      setMinSourceUsd("");
      setCooldownSecs("0");
      setSlippagePct("2");
      setSellMode("copy");
      setLadder(ladderDraftFromLegSpecs(null));
    }
    ipc.trackedWalletsList().then(setWallets).catch(() => setWallets([]));
  }, [existing]);

  const matched = useMemo(() => {
    const t = leaderInput.trim();
    if (!wallets || t === "") return null;
    return (
      wallets.find((w) => w.address === t || (w.alias ?? "").toLowerCase() === t.toLowerCase()) ??
      null
    );
  }, [wallets, leaderInput]);

  const leaderValid = existing != null || matched != null || isSolanaAddress(leaderInput);

  const copyFeedOff = existing
    ? !existing.wallet_copy_mode &&
      !(wallets?.find((w) => w.id === existing.tracked_wallet_id)?.copy_mode ?? false)
    : matched
      ? !matched.copy_mode
      : false;

  const wantLadder = sellMode === "ladder" || sellMode === "both";
  const legacyCap = existing?.max_position_sol_lamports ?? null;

  const save = async () => {
    setErr(null);
    try {
      const fixed = Number(fixedSol);
      const pct = Number(pctOfBalance);
      const leaderPct = Math.round(Number(buySizePct));
      const cashPct = Math.round(Number(balancePct));
      if (sizingMode === "fixed" && (!Number.isFinite(fixed) || fixed <= 0)) {
        throw new Error("the SOL amount per copy must be above 0");
      }
      if (sizingMode === "pct" && (!Number.isFinite(pct) || pct <= 0 || pct > 100)) {
        throw new Error("% of balance must be between 0 and 100");
      }
      if (sizingMode === "leaderSize" && (!Number.isFinite(leaderPct) || leaderPct < 1)) {
        throw new Error("% of the leader's buy must be a whole number, 1 or more");
      }
      if (sizingMode === "leaderCash" && (!Number.isFinite(cashPct) || cashPct < 1)) {
        throw new Error("balance % must be a whole number, 1 or more");
      }
      const budget = budgetSol.trim() === "" ? null : Number(budgetSol);
      if (budget != null && (!Number.isFinite(budget) || budget <= 0)) {
        throw new Error("the copy budget must be above 0 SOL (leave it empty for no cap)");
      }
      const slip = Number(slippagePct);
      if (!Number.isFinite(slip) || slip <= 0 || slip > 100) {
        throw new Error("slippage must be between 0 and 100%");
      }
      const cooldown = Math.max(0, Math.round(Number(cooldownSecs) || 0));
      if (wantLadder) {
        const lErr = validateLadderDraft(ladder, "v2");
        if (lErr) throw new Error(lErr);
      }
      const mirrorSells = sellMode === "copy" || sellMode === "both";
      const ladderSpec = wantLadder ? ladderSpecFromDraft(ladder) : null;

      setBusy(true);

      let walletId = existing?.tracked_wallet_id ?? matched?.id ?? null;
      let walletCopyMode = existing ? existing.wallet_copy_mode : (matched?.copy_mode ?? false);
      if (!existing && walletId == null) {
        const addr = leaderInput.trim();
        if (!isSolanaAddress(addr)) throw new Error("paste a valid base58 Solana wallet address");
        const created = await createTrackedWallet(addr).catch(async (e) => {
          const fresh = await ipc.trackedWalletsList().catch(() => null);
          const hit = fresh?.find((w) => w.address === addr);
          if (hit) return { id: hit.id, copy_mode: hit.copy_mode };
          throw e;
        });
        walletId = created.id;
        walletCopyMode = created.copy_mode;
      }

      const common = {
        enabled,
        sizing_mode: SIZING_TO_MODE[sizingMode],
        ...(sizingMode === "fixed" ? { fixed_sol_lamports: Math.round(fixed * 1e9) } : {}),
        ...(sizingMode === "pct" ? { pct_of_balance_bps: Math.round(pct * 100) } : {}),
        ...(sizingMode === "leaderSize" ? { buy_size_pct: leaderPct } : {}),
        ...(sizingMode === "leaderCash" ? { balance_pct: cashPct } : {}),
        single_buy_per_token: singleBuy,
        per_mint_cooldown_secs: cooldown,
        slippage_bps: Math.round(slip * 100),
        mirror_sells: mirrorSells,
      };
      if (existing) {
        const patch: SolCopyConfigPatch = {
          ...common,
          // D9: nothing writes the legacy risk cap any more; the only
          // remaining action is removing a leftover value.
          ...(dropLegacyCap ? { clear_max_position: true } : {}),
          ...(budget != null
            ? { copy_budget_lamports: Math.round(budget * 1e9) }
            : { clear_copy_budget: true }),
          ...(resetBudget ? { reset_copy_budget: true } : {}),
          ...(minSourceUsd.trim()
            ? { min_source_buy_usd: minSourceUsd.trim() }
            : { clear_min_source_buy: true }),
          ...(ladderSpec ? { default_ladder: ladderSpec as unknown as Record<string, unknown> } : { clear_default_ladder: true }),
        };
        await commands.sol.copyConfigUpdate(existing.id, patch);
      } else {
        const body: SolCopyConfigCreate = {
          tracked_wallet_id: walletId!,
          ...common,
          ...(budget != null ? { copy_budget_lamports: Math.round(budget * 1e9) } : {}),
          min_source_buy_usd: minSourceUsd.trim() || null,
          default_ladder: (ladderSpec as unknown as Record<string, unknown>) ?? null,
        };
        await commands.sol.copyConfigCreate(body);
      }
      if (enabled && walletId && !walletCopyMode) {
        await commands.sol.trackedWalletSetCopyMode(walletId, true).catch(() => {});
      }
      onSaved();
      onClose();
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async () => {
    if (!existing) return;
    setBusy(true);
    setDeleteErr(null);
    try {
      await commands.sol.copyConfigDelete(existing.id);
      setConfirmDelete(false);
      onSaved();
      onClose();
    } catch (e) {
      setDeleteErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <InlineEditor
        anchored
        columns={2}
        title={existing ? "Copy settings" : "Follow a Solana wallet"}
        subtitle={
          existing ? (
            <span className="mono">{existing.leader}</span>
          ) : (
            "Mirror another wallet's Solana buys. Paste any base58 address and tracking starts automatically."
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
              <Save size={13} /> {busy ? "Saving…" : existing ? "Save settings" : "Start following"}
            </button>
          </>
        }
      >
        <FormGroup title="Leader">
          {existing ? (
            <Field label="Leader wallet">
              <div className="mono field-static">{existing.leader}</div>
            </Field>
          ) : (
            <TextField
              label="Leader wallet"
              value={leaderInput}
              onChange={setLeaderInput}
              mono
              autoFocus
              placeholder="paste a base58 address or a tracked wallet's alias"
              hint={
                matched
                  ? `already tracked ✓ ${matched.alias?.trim() ? `${matched.alias} · ` : ""}${shortAddr(matched.address, 5, 4)}`
                  : isSolanaAddress(leaderInput)
                    ? "new wallet. Tracking starts when you save"
                    : leaderInput.trim()
                      ? "not a valid Solana address yet"
                      : "any base58 wallet works. Tracking starts automatically"
              }
            />
          )}
          <CheckField
            label="Following (copies trades while on)"
            checked={enabled}
            onChange={setEnabled}
            hint={
              enabled && copyFeedOff
                ? "this wallet's copy feed is off, so saving turns it on for you"
                : undefined
            }
          />
        </FormGroup>

        <FormGroup title="Buy sizing">
          <Field label="How each copy is sized">
            <Segmented
              options={[
                { value: "fixed", label: "Fixed SOL" },
                { value: "pct", label: "% of my balance" },
                { value: "leaderSize", label: "% of leader's buy" },
                { value: "leaderCash", label: "Match conviction" },
              ]}
              value={sizingMode}
              onChange={setSizingMode}
            />
          </Field>
          {sizingMode === "fixed" && (
            <NumField
              label="Per copy"
              unit="SOL"
              value={fixedSol}
              onChange={setFixedSol}
              placeholder="0.1"
              hint="the same amount for every copied buy"
            />
          )}
          {sizingMode === "pct" && (
            <NumField
              label="Per copy"
              unit="% of balance"
              value={pctOfBalance}
              onChange={setPctOfBalance}
              placeholder="5"
              hint="a share of this wallet's balance at buy time"
            />
          )}
          {sizingMode === "leaderSize" && (
            <NumField
              label="Per copy"
              unit="% of their buy"
              value={buySizePct}
              onChange={setBuySizePct}
              inputMode="numeric"
              placeholder="100"
              hint="100 mirrors the leader's size, 50 halves it, 200 doubles it. Skips buys we can't price."
            />
          )}
          {sizingMode === "leaderCash" && (
            <NumField
              label="Scale"
              unit="%"
              value={balancePct}
              onChange={setBalancePct}
              inputMode="numeric"
              placeholder="100"
              hint="if the leader spends 5% of their cash, spend the same share of yours (SOL + wSOL + USDC + USDT). 100 = exact mirror. Skips buys when a balance can't be read."
            />
          )}
          {legacyCap != null && !dropLegacyCap && (
            <Field label="Leftover per-token cap">
              <div className="field-static mono">
                {solText(legacyCap)} SOL per token{" "}
                <button
                  type="button"
                  className="btn xs"
                  style={{ marginLeft: 8 }}
                  disabled={busy}
                  title="This cap came from an older version of the app. Remove it: per-config budgets on the server bound the spend now."
                  onClick={() => setDropLegacyCap(true)}
                >
                  Remove on save
                </button>
              </div>
            </Field>
          )}
          {dropLegacyCap && (
            <p className="field-row-hint" style={{ margin: 0 }}>
              The old per-token cap will be removed when you save.
            </p>
          )}
        </FormGroup>

        <FormGroup title="Spending guards">
          <NumField
            label="Copy budget"
            unit="SOL"
            value={budgetSol}
            onChange={setBudgetSol}
            placeholder="no cap"
            hint="total this config may spend on buys. Once it's used up, buys stop until you raise or reset it. Sells always go through."
          />
          {existing?.copy_budget_epoch && budgetSol.trim() !== "" && (
            <Field label="Spending counted since">
              <div className="field-static">
                {new Date(existing.copy_budget_epoch).toLocaleString()}{" "}
                <button
                  type="button"
                  className="btn xs"
                  style={{ marginLeft: 8 }}
                  disabled={busy || resetBudget}
                  title="Start counting from now. Past spending stops counting against the budget."
                  onClick={() => setResetBudget(true)}
                >
                  {resetBudget ? "Resets on save" : "Reset spent"}
                </button>
              </div>
            </Field>
          )}
          <CheckField
            label="Buy each token only once"
            checked={singleBuy}
            onChange={setSingleBuy}
            hint="skip the leader's re-buys of a token this config already bought or still holds"
          />
        </FormGroup>

        <FormGroup title="Filters & execution">
          <NumField
            label="Minimum leader buy"
            unit="$"
            value={minSourceUsd}
            onChange={setMinSourceUsd}
            placeholder="any"
            hint="skip the leader's buys below this"
          />
          <NumField
            label="Cooldown per token"
            unit="s"
            value={cooldownSecs}
            onChange={setCooldownSecs}
            inputMode="numeric"
            placeholder="0"
            hint="wait this long before copying the same token again"
          />
          <NumField label="Slippage" unit="%" value={slippagePct} onChange={setSlippagePct} placeholder="2" />
        </FormGroup>

        {/* Full width: the ladder rows need the room to stay on one line. */}
        <div style={{ gridColumn: "1 / -1" }}>
        <FormGroup title="Selling">
          <Field
            label="Exit strategy"
            hint={
              sellMode === "copy"
                ? "sell in step with the leader"
                : sellMode === "ladder"
                  ? "ignore the leader's exits. Your own ladder arms on every buy"
                  : "sell with the leader AND run your own ladder on every buy"
            }
          >
            <Segmented
              options={[
                { value: "copy", label: "Sell with the leader" },
                { value: "ladder", label: "My own TP/SL" },
                { value: "both", label: "Both" },
              ]}
              value={sellMode}
              onChange={setSellMode}
            />
          </Field>
          {wantLadder && (
            <LadderSpecEditor
              dialect="v2"
              value={ladder}
              onChange={setLadder}
              disabled={busy}
            />
          )}
        </FormGroup>
        </div>

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
          Stop copying <strong>{existing?.label}</strong> and delete these settings. Past
          trades and open positions stay untouched.
        </p>
      </DangerConfirm>
    </>
  );
}
