// Callers — section 04 of the Perpetuals Bots tab. The signal sources
// the user's bot trades from: every caller visible to the account
// joined with the user's perps-venue execution subscription.
//
// v0.3.0 slice 9 (spec §G): callers are grouped by their source server
// — each group independently collapsible, exactly like the dashboard.
// The settings editor expands IN PLACE under the caller you clicked
// (spec §D), not in a popup.
//
// Caller CATALOG management (creating callers, channels, parser
// prompts) stays on the website/admin — this surface manages how the
// user's own bot executes them.

import { useCallback, useEffect, useMemo, useState } from "react";
import { Pencil, Plus, Radio, X } from "lucide-react";
import { timeAgo } from "@degenbox/ui";
import { Collapsible } from "../../components/Collapsible";
import { EmptyState, Kpi, SkeletonRows, Switch } from "../../components/ui";
import { RowWithEditor } from "../presets/CopyTradeSection";
import {
  fetchCallers,
  fetchInstructions,
  fetchSubs,
  friendlyGatewayError,
  patchSub,
  type ExecInstructionLite,
  type ExecSubscription,
  type ParserCaller,
} from "./ipc";
import { CallerSubEditor } from "./CallerSubEditor";

export function CallersSection() {
  const [callers, setCallers] = useState<ParserCaller[] | null>(null);
  const [subs, setSubs] = useState<ExecSubscription[] | null>(null);
  const [instructions, setInstructions] = useState<ExecInstructionLite[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [editorFor, setEditorFor] = useState<string | null>(null);

  const load = useCallback(async () => {
    const [cs, ss, ins] = await Promise.allSettled([
      fetchCallers(),
      fetchSubs(),
      fetchInstructions(),
    ]);
    if (cs.status === "fulfilled") {
      setCallers(cs.value);
      setErr(null);
    } else {
      setErr(friendlyGatewayError(cs.reason));
    }
    if (ss.status === "fulfilled") setSubs(ss.value);
    if (ins.status === "fulfilled") setInstructions(ins.value);
  }, []);

  useEffect(() => {
    load();
    const id = setInterval(load, 15_000);
    return () => clearInterval(id);
  }, [load]);

  // Perps-venue sub per caller_id. (Sol subs are the Solana tab's
  // business — they never render here.)
  const subByCaller = useMemo(() => {
    const m = new Map<string, ExecSubscription>();
    for (const s of subs ?? []) {
      if (s.venue === "hyperliquid") m.set(s.caller_id, s);
    }
    return m;
  }, [subs]);

  // §G: group by source server (the dashboard's caller-group split),
  // following-first inside each group, groups with follows first.
  const groups = useMemo(() => {
    if (callers === null) return null;
    const sorted = [...callers].sort((a, b) => {
      const fa = subByCaller.get(a.caller_id)?.enabled ? 1 : 0;
      const fb = subByCaller.get(b.caller_id)?.enabled ? 1 : 0;
      if (fa !== fb) return fb - fa;
      return (b.last_signal_at ?? "").localeCompare(a.last_signal_at ?? "");
    });
    const byGroup = new Map<string, ParserCaller[]>();
    for (const c of sorted) {
      const key = c.server_name?.trim() || "Other sources";
      const list = byGroup.get(key) ?? [];
      list.push(c);
      byGroup.set(key, list);
    }
    return [...byGroup.entries()].sort((a, b) => {
      const fa = a[1].filter((c) => subByCaller.get(c.caller_id)?.enabled).length;
      const fb = b[1].filter((c) => subByCaller.get(c.caller_id)?.enabled).length;
      if (fa !== fb) return fb - fa;
      return a[0].localeCompare(b[0]);
    });
  }, [callers, subByCaller]);

  const toggleFollow = async (caller: ParserCaller) => {
    const sub = subByCaller.get(caller.caller_id);
    if (!sub) {
      // First follow goes through the editor so sizing is a conscious
      // choice, not a silent 1.0× default.
      setEditorFor(caller.caller_id);
      return;
    }
    setBusyId(caller.caller_id);
    setErr(null);
    try {
      await patchSub(sub.id, { enabled: !sub.enabled });
      await load();
    } catch (e) {
      setErr(friendlyGatewayError(e));
    } finally {
      setBusyId(null);
    }
  };

  const totalCallers = callers?.length ?? 0;
  const following =
    subs === null
      ? null
      : (subs ?? []).filter((s) => s.venue === "hyperliquid" && s.enabled).length;
  const hlInstructions = useMemo(
    () => (instructions ?? []).filter((i) => i.venue === "hyperliquid"),
    [instructions],
  );
  const executed = hlInstructions.filter((i) => i.status === "executed").length;
  const lastInstruction = hlInstructions[0]?.created_at ?? null;

  return (
    <>
      {/* summary header */}
      <div className="kpi-strip">
        <Kpi
          label="Following"
          value={
            following === null || callers === null ? "…" : `${following} / ${totalCallers}`
          }
          sub="callers executing / available"
        />
        <Kpi
          label="Instructions"
          value={instructions === null ? "—" : String(hlInstructions.length)}
          sub="recent, Perpetuals"
        />
        <Kpi
          label="Executed"
          value={instructions === null ? "—" : String(executed)}
          sub={lastInstruction ? `last ${timeAgo(lastInstruction)}` : undefined}
        />
      </div>

      {err && <div className="error-box">{err}</div>}

      {groups === null && !err ? (
        <div className="card">
          <table className="table">
            <tbody>
              <SkeletonRows rows={3} cols={8} />
            </tbody>
          </table>
        </div>
      ) : (groups?.length ?? 0) === 0 ? (
        <div className="card">
          <EmptyState
            icon={<Radio size={18} />}
            title="No callers visible to your account yet"
            hint="callers are curated on the website (Scanner → Callers)"
          />
        </div>
      ) : (
        (groups ?? []).map(([groupName, list]) => {
          const followingHere = list.filter(
            (c) => subByCaller.get(c.caller_id)?.enabled,
          ).length;
          return (
            <Collapsible
              key={groupName}
              title={groupName}
              hint={
                followingHere > 0
                  ? `${followingHere} following · ${list.length}`
                  : `${list.length}`
              }
              defaultOpen={followingHere > 0}
            >
              <div className="card" style={{ marginBottom: 0 }}>
                <table className="table">
                  <thead>
                    <tr>
                      <th>Caller</th>
                      <th>Source</th>
                      <th>Follow</th>
                      <th>Sizing</th>
                      <th>Caps</th>
                      <th>Risk</th>
                      <th className="num">Last signal</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    {list.map((c) => {
                      const sub = subByCaller.get(c.caller_id) ?? null;
                      const editing = editorFor === c.caller_id;
                      return (
                        <RowWithEditor
                          key={c.id}
                          colSpan={8}
                          editor={
                            editing ? (
                              <CallerSubEditor
                                caller={c}
                                existing={sub}
                                onClose={() => setEditorFor(null)}
                                onSaved={load}
                              />
                            ) : null
                          }
                        >
                          <td>
                            <span className="inline-flex items-center gap-2">
                              {c.avatar_url && (
                                <img
                                  src={c.avatar_url}
                                  alt=""
                                  width={18}
                                  height={18}
                                  style={{ borderRadius: 2 }}
                                />
                              )}
                              <span>
                                <strong>{c.display_name}</strong>
                                {!c.enabled && (
                                  <span
                                    className="badge warn"
                                    style={{ marginLeft: 6 }}
                                    title="The parser has this caller switched off, so no new signals come in"
                                  >
                                    parser off
                                  </span>
                                )}
                                <div className="mono text-[10.5px] text-ink-4">
                                  {c.caller_id}
                                </div>
                              </span>
                            </span>
                          </td>
                          <td className="mono text-[11px] text-ink-3">
                            {c.server_name ?? typeLabel(c)}
                          </td>
                          <td>
                            <Switch
                              on={sub?.enabled ?? false}
                              disabled={busyId !== null}
                              title={
                                sub
                                  ? sub.enabled
                                    ? "Stop executing (settings kept)"
                                    : "Resume executing (may conflict with an active copy-trade follow)"
                                  : "Follow this caller (opens the settings)"
                              }
                              onToggle={() => toggleFollow(c)}
                            />
                          </td>
                          <td className="mono text-[11.5px]">{sizingSummary(sub)}</td>
                          <td className="mono text-[10.5px] text-ink-3">
                            {capsSummary(sub)}
                          </td>
                          <td className="mono text-[10.5px] text-ink-3">
                            {riskSummary(sub)}
                          </td>
                          <td className="num">{timeAgo(c.last_signal_at)}</td>
                          <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                            <button
                              className={`btn sm ${editing ? "active" : ""}`}
                              disabled={busyId !== null}
                              aria-expanded={editing}
                              onClick={() =>
                                setEditorFor(editing ? null : c.caller_id)
                              }
                            >
                              {editing ? (
                                <X size={12} />
                              ) : sub ? (
                                <Pencil size={12} />
                              ) : (
                                <Plus size={12} />
                              )}{" "}
                              {editing ? "Close" : sub ? "Edit" : "Follow"}
                            </button>
                          </td>
                        </RowWithEditor>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            </Collapsible>
          );
        })
      )}
    </>
  );
}

// ─── bits ──────────────────────────────────────────────────────────

function typeLabel(c: ParserCaller): string {
  switch (c.caller_type) {
    case "user":
      return "discord user";
    case "role":
      return c.role_name ? `role · ${c.role_name}` : "discord role";
    case "wallet":
      return "wallet";
    case "telegram":
      return c.telegram_username ? `tg · @${c.telegram_username}` : "telegram";
    case "twitter":
      return c.twitter_handle ? `x · @${c.twitter_handle}` : "twitter";
    default:
      return c.caller_type;
  }
}

function sizingSummary(sub: ExecSubscription | null): string {
  if (!sub) return "—";
  if (sub.sizing_mode === 1) {
    const pct = sub.sizing_pct_equity_bps != null ? sub.sizing_pct_equity_bps / 100 : null;
    const tiers = [sub.size_low_percent, sub.size_normal_percent, sub.size_high_percent]
      .filter((v) => v != null);
    if (pct != null) return `${pct}% equity`;
    if (tiers.length > 0) return "tiered % equity";
    return "% equity";
  }
  if (sub.size_usd_override != null) return `$${Number(sub.size_usd_override)} fixed`;
  // Same percent language as the editor: 100% = the caller's size.
  const mult = Number(sub.size_multiplier);
  return Number.isFinite(mult)
    ? `${Number((mult * 100).toFixed(2))}% of call`
    : `×${sub.size_multiplier}`;
}

function capsSummary(sub: ExecSubscription | null): string {
  if (!sub) return "—";
  const parts: string[] = [];
  if (sub.max_size_usd != null) parts.push(`size $${Number(sub.max_size_usd)}`);
  if (sub.max_position_usd != null) parts.push(`pos $${Number(sub.max_position_usd)}`);
  if (sub.leverage_override != null) parts.push(`${sub.leverage_override}× lev`);
  else if (sub.leverage_cap != null) parts.push(`≤${sub.leverage_cap}× lev`);
  return parts.length > 0 ? parts.join(" · ") : "none";
}

function riskSummary(sub: ExecSubscription | null): string {
  if (!sub) return "—";
  const parts: string[] = [];
  if (sub.manual_sl_action === 1) parts.push(`SL ${sub.manual_sl_pct ?? "?"}%`);
  if (sub.manual_sl_action === 2) parts.push(`trail ${sub.manual_sl_pct ?? "?"}%`);
  if (sub.drawdown_stop_pct != null) parts.push(`dd ${sub.drawdown_stop_pct}%`);
  if (sub.skip_dca) parts.push("no DCA");
  if (sub.market_filter_mode === 1) parts.push(`allow ${sub.market_filter_list.length}`);
  if (sub.market_filter_mode === 2) parts.push(`block ${sub.market_filter_list.length}`);
  return parts.length > 0 ? parts.join(" · ") : "defaults";
}
