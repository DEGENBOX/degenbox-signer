// Copy trade (Solana) — section 04 of the Solana Bots tab. Full CRUD
// over the Sol copy configs.
//
// v0.3.0 slice 9: the editor expands IN PLACE — a full-width row right
// under the leader you clicked (spec §D); "Follow a wallet" opens the
// same editor directly under the header button. The "Risk cap" column
// is gone with the setting (D9); the sell strategy summarises the
// shared ladder format.

import { useMemo, useState, type ReactNode } from "react";
import { Pencil, Plus, UserPlus, X } from "lucide-react";
import { fmtSol, shortAddr, timeAgo } from "@degenbox/ui";
import { EmptyState, Kpi, SkeletonRows, Switch } from "../../components/ui";
import { summarizeStoredLadder } from "../../components/LadderSpecEditor";
import {
  followCopyConfig,
  ipc,
  sizingSummary,
  unfollowCopyConfig,
  type SolCopyConfigFull,
} from "./ipc";
import type { CopyTrade } from "./useCopyTrade";
import { CopyConfigEditor } from "./CopyConfigEditor";

export function CopyTradeSection({ copy }: { copy: CopyTrade }) {
  // Data (rows/stats/summary + the 15 s poll) is owned by the parent
  // tab via useCopyTrade so the Running-now section reads the same
  // snapshot. This component keeps the editing/toggling state.
  const { rows, stats, summary, summaryErr, err, setErr, reload: load } = copy;
  const [busyId, setBusyId] = useState<string | null>(null);
  /** null = closed · "new" = create · config id = edit that row. */
  const [editorFor, setEditorFor] = useState<string | null>(null);

  const statFor = (id: string) =>
    stats?.find((s) => s.id === id && s.venue === "solana") ?? null;

  const perWallet = useMemo(() => {
    const m = new Map<string, { mirrored: string | null; published: number }>();
    for (const w of summary?.per_wallet ?? []) {
      m.set(w.wallet_address, {
        mirrored: w.mirrored_sol_lamports,
        published: w.intents_published,
      });
    }
    return m;
  }, [summary]);

  const toggleFollow = async (c: SolCopyConfigFull) => {
    setBusyId(c.id);
    setErr(null);
    try {
      // W2.3 follow endpoints; fall back to the PATCH the shipped
      // signer already uses if the alias prefix isn't deployed yet.
      try {
        if (c.enabled) await unfollowCopyConfig(c.id);
        else await followCopyConfig(c.id);
      } catch {
        await ipc.solCopyConfigUpdate(c.id, { enabled: !c.enabled });
      }
      // Lockstep: a live follow needs the wallet's copy feed on.
      if (!c.enabled && !c.wallet_copy_mode) {
        await ipc.trackedWalletSetCopyMode(c.tracked_wallet_id, true).catch(() => {});
      }
      await load();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusyId(null);
    }
  };

  const active = rows?.filter((c) => c.enabled).length ?? 0;
  const copied24h =
    stats?.filter((s) => s.venue === "solana").reduce((a, s) => a + s.copied_24h, 0) ??
    null;
  const s = summary?.summary ?? null;

  return (
    <>
      {/* summary header */}
      <div className="kpi-strip">
        <Kpi
          label="Leaders"
          value={rows === null ? "…" : `${active} / ${rows.length}`}
          sub="following / set up"
        />
        <Kpi
          label="Copies 24h"
          value={copied24h === null ? "—" : String(copied24h)}
          sub={s ? `${s.intents_published} sent all-time` : undefined}
        />
        <Kpi
          label="Mirrored 24h"
          value={solOf(s?.mirrored_sol_lamports_24h)}
          sub={summaryErr ? "totals unavailable right now" : undefined}
        />
        <Kpi label="Mirrored 7d" value={solOf(s?.mirrored_sol_lamports_7d)} />
        <Kpi
          label="Mirrored all"
          value={solOf(s?.mirrored_sol_lamports_all)}
          sub={
            s
              ? `${s.intents_rejected} skipped · last ${timeAgo(s.last_intent_at)}`
              : undefined
          }
        />
      </div>

      <div className="card">
        <div className="card-title">
          Leader wallets
          <span className="right">
            <button
              className={`btn sm ${editorFor === "new" ? "active" : ""}`}
              aria-expanded={editorFor === "new"}
              onClick={() => setEditorFor(editorFor === "new" ? null : "new")}
            >
              {editorFor === "new" ? <X size={12} /> : <Plus size={12} />} Follow a wallet
            </button>
          </span>
        </div>
        {err && <div className="error-box">{err}</div>}

        {editorFor === "new" && (
          <CopyConfigEditor
            existing={null}
            onClose={() => setEditorFor(null)}
            onSaved={load}
          />
        )}

        <table className="table">
          <thead>
            <tr>
              <th>Leader</th>
              <th>Follow</th>
              <th>Sizing</th>
              <th>Filters</th>
              <th>Selling</th>
              <th className="num">Copies 24h</th>
              <th className="num">Mirrored</th>
              <th className="num">Last copy</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows === null ? (
              <SkeletonRows rows={2} cols={9} />
            ) : rows.length === 0 ? (
              <tr>
                <td colSpan={9}>
                  <EmptyState
                    icon={<UserPlus size={18} />}
                    title="Not following anyone yet"
                    hint='click "Follow a wallet" and paste an address to mirror its buys'
                  />
                </td>
              </tr>
            ) : (
              rows.map((c) => {
                const st = statFor(c.id);
                const pw = perWallet.get(c.leader);
                const editing = editorFor === c.id;
                return (
                  <RowWithEditor
                    key={c.id}
                    colSpan={9}
                    editor={
                      editing ? (
                        <CopyConfigEditor
                          existing={c}
                          onClose={() => setEditorFor(null)}
                          onSaved={load}
                        />
                      ) : null
                    }
                  >
                    <td>
                      <strong>{c.label}</strong>
                      <div className="mono text-[10.5px] text-ink-4">
                        {shortAddr(c.leader, 5, 4)}
                      </div>
                    </td>
                    <td>
                      <span className="inline-flex items-center gap-2">
                        <Switch
                          on={c.enabled}
                          disabled={busyId !== null}
                          title={c.enabled ? "Stop following (settings kept)" : "Follow"}
                          onToggle={() => toggleFollow(c)}
                        />
                        {c.enabled && !c.wallet_copy_mode && (
                          <span
                            className="badge warn"
                            title="This wallet's copy feed is off. Open the settings and save to fix it"
                          >
                            feed off
                          </span>
                        )}
                      </span>
                    </td>
                    <td className="mono text-[11.5px]">{sizingSummary(c)}</td>
                    <td className="mono text-[10.5px] text-ink-3">{filterSummary(c)}</td>
                    <td>
                      <SellsCell c={c} />
                    </td>
                    <td className="num">{st?.copied_24h ?? 0}</td>
                    <td
                      className="num mono text-[11.5px]"
                      title={pw ? `${pw.published} copies sent` : undefined}
                    >
                      {pw?.mirrored != null ? `${fmtSol(pw.mirrored)} SOL` : "—"}
                    </td>
                    <td className="num">{timeAgo(st?.last_copy_at ?? null)}</td>
                    <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                      <button
                        className={`btn sm ${editing ? "active" : ""}`}
                        disabled={busyId !== null}
                        aria-expanded={editing}
                        onClick={() => setEditorFor(editing ? null : c.id)}
                      >
                        {editing ? <X size={12} /> : <Pencil size={12} />}{" "}
                        {editing ? "Close" : "Edit"}
                      </button>
                    </td>
                  </RowWithEditor>
                );
              })
            )}
          </tbody>
        </table>
      </div>
    </>
  );
}

// ─── bits ──────────────────────────────────────────────────────────

/** A table row plus, when open, a full-width editor row attached right
 * under it (spec §D — the editor sits where you clicked). */
export function RowWithEditor({
  children,
  editor,
  colSpan,
}: {
  children: ReactNode;
  editor: ReactNode | null;
  colSpan: number;
}) {
  return (
    <>
      <tr className={editor ? "row-editing" : undefined}>{children}</tr>
      {editor && (
        <tr className="row-editor">
          <td colSpan={colSpan}>{editor}</td>
        </tr>
      )}
    </>
  );
}

/** Decimal-as-string lamports → "1.25 SOL" / "—". */
function solOf(v: string | null | undefined): string {
  if (v == null) return "—";
  const n = Number(v);
  if (!Number.isFinite(n)) return "—";
  if (n === 0) return "0 SOL";
  return `${fmtSol(n)} SOL`;
}

function filterSummary(c: SolCopyConfigFull): string {
  const parts: string[] = [];
  if (c.min_source_buy_usd != null && c.min_source_buy_usd !== "") {
    parts.push(`min $${Number(c.min_source_buy_usd)}`);
  }
  if (c.per_mint_cooldown_secs > 0) parts.push(`cd ${c.per_mint_cooldown_secs}s`);
  parts.push(`slip ${(c.slippage_bps / 100).toFixed(1)}%`);
  if (c.max_position_sol_lamports != null) {
    // Legacy cap left over from an older version — visible, removable
    // in the editor, never written any more.
    parts.push(`legacy cap ${fmtSol(c.max_position_sol_lamports)} SOL`);
  }
  return parts.join(" · ");
}

function SellsCell({ c }: { c: SolCopyConfigFull }) {
  const ladderLine = summarizeStoredLadder(c.default_ladder);
  if (!c.mirror_sells && !ladderLine) {
    return <span className="text-[11px] font-mono text-ink-4">manual</span>;
  }
  return (
    <span className="inline-flex items-center gap-1.5 flex-wrap">
      {c.mirror_sells && (
        <span className="badge ok" title="sells in step with the leader">
          with leader
        </span>
      )}
      {ladderLine && (
        <span
          className="text-[10.5px] font-mono text-ink-3"
          title="Your own TP/SL ladder, armed on every copied buy"
        >
          {ladderLine}
        </span>
      )}
    </span>
  );
}
