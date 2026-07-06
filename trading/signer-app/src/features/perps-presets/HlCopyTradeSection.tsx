// Copy-Trade — section 02 of the Perpetuals Presets tab. Full CRUD
// over the HL copy configs (restructured from pages/Copytrade.tsx,
// hyperliquid branch — same typed commands, W3.2 presentation). The
// summary header rides `GET /api/hyperliquid/copy-trade/summary`
// (mirrored-USD windows + per-wallet rollup); the follow toggle uses
// the canonical follow/unfollow endpoints with a PATCH fallback for
// older gateways. The single-follow invariant (one active follow per
// account — copy target OR caller) is server-enforced; 409s render as
// readable sentences.

import { useCallback, useEffect, useMemo, useState } from "react";
import { Pencil, Plus, UserPlus, X } from "lucide-react";
import { fmtUsd, shortAddr, timeAgo } from "@degenbox/ui";
import { EmptyState, Kpi, SkeletonRows, Switch } from "../../components/ui";
import { RowWithEditor } from "../presets/CopyTradeSection";
import {
  fetchHlCopySummary,
  followHlConfig,
  friendlyGatewayError,
  ipc,
  unfollowHlConfig,
  type CopytradeConfig,
  type HlCopyConfigFull,
  type HlCopySummaryView,
} from "./ipc";
import { HlCopyConfigEditor } from "./HlCopyConfigEditor";

export function HlCopyTradeSection() {
  // Editable rows = the full-field command; copies-24h / last-copy
  // stats = the venue-merged summary command; volume = gateway summary.
  const [rows, setRows] = useState<HlCopyConfigFull[] | null>(null);
  const [stats, setStats] = useState<CopytradeConfig[] | null>(null);
  const [summary, setSummary] = useState<HlCopySummaryView | null>(null);
  const [summaryErr, setSummaryErr] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [editor, setEditor] = useState<HlCopyConfigFull | null | "new">(null);

  const load = useCallback(async () => {
    const [full, st, sum] = await Promise.allSettled([
      ipc.hlCopyConfigsFull(),
      ipc.copytradeConfigs(),
      fetchHlCopySummary(),
    ]);
    if (full.status === "fulfilled") {
      setRows(full.value);
      setErr(null);
    } else {
      setErr(friendlyGatewayError(full.reason));
    }
    if (st.status === "fulfilled") setStats(st.value);
    if (sum.status === "fulfilled") {
      setSummary(sum.value);
      setSummaryErr(false);
    } else {
      // Endpoint dark (non-subscriber / older gateway) — keep the last
      // value, flag it; the table still works without it.
      setSummaryErr(true);
    }
  }, []);

  useEffect(() => {
    load();
    const id = setInterval(load, 15_000);
    return () => clearInterval(id);
  }, [load]);

  const statFor = (id: string) =>
    stats?.find((s) => s.id === id && s.venue === "hyperliquid") ?? null;

  const perWallet = useMemo(() => {
    const m = new Map<string, { mirrored: string | null; intents: number }>();
    for (const w of summary?.per_wallet ?? []) {
      m.set(w.target_wallet.toLowerCase(), {
        mirrored: w.mirrored_usd,
        intents: w.intents_count,
      });
    }
    return m;
  }, [summary]);

  const toggleFollow = async (c: HlCopyConfigFull) => {
    setBusyId(c.id);
    setErr(null);
    try {
      // Canonical follow endpoints (enforce single-follow + clear
      // exclusive_follow on unfollow); fall back to the plain PATCH
      // the shipped signer already uses if the route isn't deployed.
      try {
        if (c.enabled) await unfollowHlConfig(c.id);
        else await followHlConfig(c.id);
      } catch (e) {
        // A 409 is a real answer (another follow active) — surface it
        // instead of falling back into a guard-free PATCH.
        if (String(e).includes("409")) throw e;
        await ipc.hlCopyConfigUpdate(c.id, { enabled: !c.enabled });
      }
      await load();
    } catch (e) {
      setErr(friendlyGatewayError(e));
    } finally {
      setBusyId(null);
    }
  };

  const active = rows?.filter((c) => c.enabled).length ?? 0;
  const copied24h =
    stats
      ?.filter((s) => s.venue === "hyperliquid")
      .reduce((a, s) => a + s.copied_24h, 0) ?? null;
  const s = summary?.summary ?? null;

  return (
    <>
      {/* summary header */}
      <div className="kpi-strip">
        <Kpi
          label="Leaders"
          value={rows === null ? "…" : `${active} / ${rows.length}`}
          sub="following / configured"
        />
        <Kpi
          label="Copies 24h"
          value={copied24h === null ? "—" : String(copied24h)}
          sub={s ? `${s.intents_confirmed} confirmed all-time` : undefined}
        />
        <Kpi
          label="Mirrored 24h"
          value={usdOf(s?.mirrored_usd_24h)}
          sub={summaryErr ? "summary unavailable" : undefined}
        />
        <Kpi label="Mirrored 7d" value={usdOf(s?.mirrored_usd_7d)} />
        <Kpi
          label="Mirrored all"
          value={usdOf(s?.mirrored_usd_all)}
          sub={
            s
              ? `${s.intents_rejected} rejected · last ${timeAgo(s.last_intent_at)}`
              : undefined
          }
        />
      </div>

      <div className="card">
        <div className="card-title">
          Leader wallets
          <span className="right">
            <button
              className={`btn sm ${editor === "new" ? "active" : ""}`}
              aria-expanded={editor === "new"}
              onClick={() => setEditor(editor === "new" ? null : "new")}
            >
              {editor === "new" ? <X size={12} /> : <Plus size={12} />} Follow a wallet
            </button>
          </span>
        </div>
        {err && <div className="error-box">{err}</div>}

        {editor === "new" && (
          <HlCopyConfigEditor
            existing={null}
            onClose={() => setEditor(null)}
            onSaved={load}
          />
        )}
        <table className="table">
          <thead>
            <tr>
              <th>Leader</th>
              <th>Follow</th>
              <th>Mode</th>
              <th className="num">Caps</th>
              <th>Filters</th>
              <th>Closes / TP / SL</th>
              <th className="num">Copies 24h</th>
              <th className="num">Mirrored</th>
              <th className="num">Last copy</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows === null ? (
              <SkeletonRows rows={2} cols={10} />
            ) : rows.length === 0 ? (
              <tr>
                <td colSpan={10}>
                  <EmptyState
                    icon={<UserPlus size={18} />}
                    title="Not following anyone yet"
                    hint='click "Follow a wallet" and paste a 0x… address to mirror its perp trades'
                  />
                </td>
              </tr>
            ) : (
              rows.map((c) => {
                const st = statFor(c.id);
                const pw = perWallet.get(c.target_wallet.toLowerCase());
                const editing = editor !== "new" && editor?.id === c.id;
                return (
                  <RowWithEditor
                    key={c.id}
                    colSpan={10}
                    editor={
                      editing ? (
                        <HlCopyConfigEditor
                          existing={c}
                          onClose={() => setEditor(null)}
                          onSaved={load}
                        />
                      ) : null
                    }
                  >
                    <td className="mono">
                      <strong>{shortAddr(c.target_wallet, 6, 4)}</strong>
                    </td>
                    <td>
                      <Switch
                        on={c.enabled}
                        disabled={busyId !== null}
                        title={
                          c.enabled
                            ? "Stop following (settings kept)"
                            : "Follow (refused with a clear message if another follow is active)"
                        }
                        onToggle={() => toggleFollow(c)}
                      />
                    </td>
                    <td className="mono text-[11.5px]">{modeSummary(c)}</td>
                    <td className="num mono text-[11.5px]">{capsSummary(c)}</td>
                    <td className="mono text-[10.5px] text-ink-3">
                      {filterSummary(c)}
                    </td>
                    <td>
                      <ExitsCell c={c} />
                    </td>
                    <td className="num">{st?.copied_24h ?? 0}</td>
                    <td
                      className="num mono text-[11.5px]"
                      title={pw ? `${pw.intents} copy intents` : undefined}
                    >
                      {pw?.mirrored != null ? fmtUsd(Number(pw.mirrored)) : "—"}
                    </td>
                    <td className="num">{timeAgo(st?.last_copy_at ?? null)}</td>
                    <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                      <button
                        className={`btn sm ${editing ? "active" : ""}`}
                        disabled={busyId !== null}
                        aria-expanded={editing}
                        onClick={() => setEditor(editing ? null : c)}
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

/** Decimal-as-string USD → "$1,234" / "—". */
function usdOf(v: string | null | undefined): string {
  if (v == null) return "—";
  const n = Number(v);
  if (!Number.isFinite(n)) return "—";
  return fmtUsd(n);
}

function modeSummary(c: HlCopyConfigFull): string {
  const scale = Number(c.scale_factor);
  const pctTxt = Number.isFinite(scale) ? `${Math.round(scale * 100)}%` : c.scale_factor;
  switch (c.follow_mode ?? 0) {
    case 1:
      // Legacy mode — equals "% of leader's size" at 100.
      return "100% of leader's size";
    case 2:
      return `balance-matched · ${pctTxt}`;
    case 3:
      return `${usdOf(c.fixed_size_usd)} per copy`;
    default:
      return `${pctTxt} of leader's size`;
  }
}

function capsSummary(c: HlCopyConfigFull): string {
  const parts: string[] = [];
  if (c.max_position_usd != null) parts.push(fmtUsd(Number(c.max_position_usd)));
  if (c.leverage_cap != null) parts.push(`≤${c.leverage_cap}×`);
  return parts.length > 0 ? parts.join(" · ") : "—";
}

function filterSummary(c: HlCopyConfigFull): string {
  const parts: string[] = [];
  if ((c.coin_allowlist ?? []).length > 0) {
    parts.push(`${c.coin_allowlist.length} coin${c.coin_allowlist.length === 1 ? "" : "s"}`);
  }
  if (c.min_fill_usd != null) parts.push(`min $${Number(c.min_fill_usd)}`);
  if (c.drawdown_stop_pct != null && c.drawdown_stop_pct > 0) {
    parts.push(`dd ${c.drawdown_stop_pct}%`);
  }
  parts.push(`slip ${((c.slippage_limit_bps ?? 200) / 100).toFixed(1)}%`);
  return parts.join(" · ");
}

function ExitsCell({ c }: { c: HlCopyConfigFull }) {
  const sl = c.sl_placement_strategy ?? 0;
  const tp = c.tp_placement_strategy ?? 0;
  return (
    <span className="inline-flex items-center gap-1 flex-wrap">
      {c.mirror_closes && (
        <span className="badge ok" title="closes proportionally when the leader closes">
          closes
        </span>
      )}
      {sl > 0 && (
        <span
          className="badge"
          title={
            sl === 1
              ? "stop-loss mirrors the leader's"
              : sl === 2
                ? `fixed stop ${c.sl_placement_pct ?? "?"}% from entry`
                : `trailing stop ${c.sl_placement_pct ?? "?"}%`
          }
        >
          {sl === 1 ? "SL copy" : sl === 2 ? `SL ${c.sl_placement_pct ?? "?"}%` : "SL trail"}
        </span>
      )}
      {tp > 0 && (
        <span
          className="badge"
          title={
            tp === 1
              ? "take-profits mirror the leader's"
              : `${(c.tp_levels_json ?? []).length} fixed TP levels`
          }
        >
          {tp === 1 ? "TP copy" : `TP ×${(c.tp_levels_json ?? []).length}`}
        </span>
      )}
      {!c.mirror_closes && sl === 0 && tp === 0 && (
        <span className="text-[11px] font-mono text-ink-4">manual</span>
      )}
    </span>
  );
}
