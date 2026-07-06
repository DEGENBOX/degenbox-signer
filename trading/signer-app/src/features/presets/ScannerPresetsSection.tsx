// Scanner presets (auto-buy) — section 03 of the Solana Bots tab.
//
// v0.3.0 slice 9:
//  * Split into two independently collapsible groups exactly like the
//    web dashboard (spec §C): "DegenBox presets" (public/curated) and
//    "Your presets".
//  * The Execution / Bots editors expand IN PLACE, directly under the
//    preset card that opened them (spec §D) — never detached.
//  * The sell strategy renders from the v2 `bot_config.ladder` (the key
//    execution actually compiles), with a legacy fallback readout.
//
// Per preset: read-only filter summary (the rule EDITOR stays on the
// website), the in-app-editable execution + sell strategy, and the
// bot↔preset assignment matrix.

import { useCallback, useEffect, useMemo, useState } from "react";
import { ExternalLink, Link2, SlidersHorizontal } from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { fmtSol, shortAddr } from "@degenbox/ui";
import { Collapsible } from "../../components/Collapsible";
import { EmptyState, SkeletonRows } from "../../components/ui";
import {
  WEB_PRESETS_URL,
  fetchAlphaPresets,
  ipc,
  type AlphaPresetFull,
  type ClientInfo,
  type ClientPreset,
} from "./ipc";
import {
  hasExecutionConfig,
  parseBotConfig,
  summarizeExecution,
} from "./botConfig";
import {
  ladderDraftFromLegacyBotConfig,
  summarizeLadderDraft,
  summarizeStoredLadder,
} from "../../components/LadderSpecEditor";
import { summarizeRules } from "./ruleSummary";
import { ExecutionEditor } from "./ExecutionEditor";
import { AssignmentEditor } from "./AssignmentEditor";

/** One client's assignment row for a preset. */
export interface PresetAssignment {
  client: ClientInfo;
  row: ClientPreset;
}

type OpenEditor = { presetId: string; kind: "exec" | "assign" } | null;

export function ScannerPresetsSection() {
  const [presets, setPresets] = useState<AlphaPresetFull[] | null>(null);
  const [clients, setClients] = useState<ClientInfo[] | null>(null);
  const [assignments, setAssignments] = useState<Map<string, PresetAssignment[]>>(
    new Map(),
  );
  const [err, setErr] = useState<string | null>(null);
  const [assignErr, setAssignErr] = useState<string | null>(null);
  const [open, setOpen] = useState<OpenEditor>(null);

  const load = useCallback(async () => {
    const [pr, cl] = await Promise.allSettled([fetchAlphaPresets(), ipc.clientsList()]);
    if (pr.status === "fulfilled") {
      setPresets(pr.value);
      setErr(null);
    } else {
      setErr(String(pr.reason));
    }
    if (cl.status !== "fulfilled") {
      setClients([]);
      setAssignErr(String(cl.reason));
      return;
    }
    // Assignment truth lives per CLIENT on the gateway — fan out over
    // the Solana clients that exist server-side, then pivot per preset.
    const sol = cl.value.filter((c) => c.chain === "sol" && c.gateway != null);
    setClients(sol);
    const lists = await Promise.allSettled(
      sol.map((c) => ipc.clientPresetsList(c.gateway!.id)),
    );
    const byPreset = new Map<string, PresetAssignment[]>();
    const errs: string[] = [];
    lists.forEach((res, i) => {
      const client = sol[i];
      if (res.status === "fulfilled") {
        for (const row of res.value) {
          const list = byPreset.get(row.preset_id) ?? [];
          list.push({ client, row });
          byPreset.set(row.preset_id, list);
        }
      } else {
        errs.push(`${client.label ?? shortAddr(client.address)}: ${res.reason}`);
      }
    });
    setAssignments(byPreset);
    setAssignErr(errs.length > 0 ? errs.join(" · ") : null);
  }, []);

  useEffect(() => {
    load();
    const id = setInterval(load, 30_000);
    return () => clearInterval(id);
  }, [load]);

  const groups = useMemo(() => {
    if (!presets) return null;
    // Active first, then auto-buy-configured, then name.
    const sorted = [...presets].sort((a, b) => {
      if (a.is_active !== b.is_active) return a.is_active ? -1 : 1;
      const ax = hasExecutionConfig(parseBotConfig(a.bot_config));
      const bx = hasExecutionConfig(parseBotConfig(b.bot_config));
      if (ax !== bx) return ax ? -1 : 1;
      return a.name.localeCompare(b.name);
    });
    return {
      pub: sorted.filter((p) => p.is_public),
      own: sorted.filter((p) => !p.is_public),
    };
  }, [presets]);

  const renderCards = (list: AlphaPresetFull[]) => (
    <div className="grid gap-3">
      {list.map((p) => (
        <div key={p.id}>
          <PresetCard
            preset={p}
            assignments={assignments.get(p.id) ?? []}
            execOpen={open?.presetId === p.id && open.kind === "exec"}
            assignOpen={open?.presetId === p.id && open.kind === "assign"}
            onEditExec={() =>
              setOpen(
                open?.presetId === p.id && open.kind === "exec"
                  ? null
                  : { presetId: p.id, kind: "exec" },
              )
            }
            onEditAssign={() =>
              setOpen(
                open?.presetId === p.id && open.kind === "assign"
                  ? null
                  : { presetId: p.id, kind: "assign" },
              )
            }
          />
          {open?.presetId === p.id && open.kind === "exec" && (
            <ExecutionEditor preset={p} onClose={() => setOpen(null)} onSaved={load} />
          )}
          {open?.presetId === p.id && open.kind === "assign" && (
            <AssignmentEditor
              preset={p}
              clients={clients ?? []}
              assignments={assignments.get(p.id) ?? []}
              onClose={() => setOpen(null)}
              onChanged={load}
            />
          )}
        </div>
      ))}
    </div>
  );

  return (
    <>
      <div className="flex items-center justify-between gap-3 mb-3">
        <p className="page-sub" style={{ margin: 0 }}>
          Filters are edited on the website. This device owns the buying, the sell
          strategy and which bots run each preset.
        </p>
        <button
          className="btn sm"
          style={{ whiteSpace: "nowrap" }}
          onClick={() => openUrl(WEB_PRESETS_URL).catch(() => {})}
          title={WEB_PRESETS_URL}
        >
          <ExternalLink size={12} /> Edit filters on the website
        </button>
      </div>

      {err && <div className="error-box">{err}</div>}
      {assignErr && (
        <div className="error-box">some bot assignments didn't load: {assignErr}</div>
      )}

      {groups === null && !err ? (
        <div className="card">
          <table className="table">
            <tbody>
              <SkeletonRows rows={3} cols={3} />
            </tbody>
          </table>
        </div>
      ) : (groups?.pub.length ?? 0) + (groups?.own.length ?? 0) === 0 ? (
        <div className="card">
          <EmptyState
            icon={<SlidersHorizontal size={18} />}
            title="No scanner presets yet"
            hint="create one on the website, then set up buying and bots here"
          />
        </div>
      ) : (
        <>
          <Collapsible
            title="DegenBox presets"
            hint={`${groups!.pub.length}`}
            defaultOpen={groups!.pub.length > 0}
          >
            {groups!.pub.length === 0 ? (
              <p className="page-sub" style={{ margin: "4px 0 8px" }}>
                Nothing curated is shared with your account right now.
              </p>
            ) : (
              renderCards(groups!.pub)
            )}
          </Collapsible>
          <Collapsible
            title="Your presets"
            hint={`${groups!.own.length}`}
            defaultOpen
          >
            {groups!.own.length === 0 ? (
              <p className="page-sub" style={{ margin: "4px 0 8px" }}>
                You haven't made a preset yet. Build one on the website and it shows up
                here.
              </p>
            ) : (
              renderCards(groups!.own)
            )}
          </Collapsible>
        </>
      )}
    </>
  );
}

// ─── one preset card ───────────────────────────────────────────────

function PresetCard({
  preset,
  assignments,
  execOpen,
  assignOpen,
  onEditExec,
  onEditAssign,
}: {
  preset: AlphaPresetFull;
  assignments: PresetAssignment[];
  execOpen: boolean;
  assignOpen: boolean;
  onEditExec: () => void;
  onEditAssign: () => void;
}) {
  const cfg = useMemo(() => parseBotConfig(preset.bot_config), [preset.bot_config]);
  const autotrade = hasExecutionConfig(cfg);
  const groups = useMemo(
    () => summarizeRules(preset.rules?.rules),
    [preset.rules],
  );
  const execLine = summarizeExecution(cfg);
  // Sell-strategy readout: the v2 ladder if present, else the legacy
  // keys older builds wrote (execution never read those — the editor
  // migrates them to `ladder` on the next save).
  const ladderLine =
    summarizeStoredLadder(cfg.ladder) ||
    (cfg.takeProfits.length > 0 || cfg.stopLossPct != null
      ? `${summarizeLadderDraft(
          ladderDraftFromLegacyBotConfig(cfg.takeProfits, cfg.stopLossPct),
        )} (legacy, resave to apply)`
      : "");
  const enabledAssignments = assignments.filter((a) => a.row.enabled);

  return (
    <div className="card" style={{ marginBottom: 0 }}>
      {/* head row */}
      <div className="flex items-center gap-2 flex-wrap">
        <span
          aria-hidden
          className="inline-block w-2 h-2 rounded-sm flex-shrink-0"
          style={{ background: preset.color ?? "rgb(var(--ink-4))" }}
        />
        <strong className="text-[13.5px] tracking-tight">{preset.name}</strong>
        <span className={`badge ${preset.is_active ? "ok" : ""}`}>
          {preset.is_active ? "active" : "off"}
        </span>
        {preset.is_public && <span className="badge preview">curated</span>}
        {autotrade ? (
          <span className="badge ok">auto-buy</span>
        ) : (
          <span className="badge">alerts only</span>
        )}
        <span className="ml-auto flex items-center gap-1.5">
          <button
            className={`btn sm ${execOpen ? "active" : ""}`}
            onClick={onEditExec}
            aria-expanded={execOpen}
            title="Buy size, slippage, tip and the TP/SL ladder"
          >
            <SlidersHorizontal size={12} /> Execution
          </button>
          <button
            className={`btn sm ${assignOpen ? "active" : ""}`}
            onClick={onEditAssign}
            aria-expanded={assignOpen}
            title="Which of your bots run this preset (+ per-bot overrides)"
          >
            <Link2 size={12} /> Bots
            {assignments.length > 0 ? ` (${enabledAssignments.length}/${assignments.length})` : ""}
          </button>
        </span>
      </div>

      {/* filter summary — read-only */}
      <div className="mt-3 grid gap-1.5">
        {groups.length === 0 ? (
          <span className="text-[11.5px] font-mono text-ink-4">
            no filters (matches every signal)
          </span>
        ) : (
          groups.map((g) => (
            <div key={g.section} className="flex items-baseline gap-2 flex-wrap">
              <span className="hud-label w-[84px] flex-shrink-0">{g.section}</span>
              <span className="flex flex-wrap gap-1">
                {g.items.map((item, i) => (
                  <span
                    key={`${g.section}-${i}`}
                    className="inline-flex items-center px-1.5 h-[18px] rounded-sm border border-line/40 bg-card/60 text-[10.5px] font-mono text-ink-2 whitespace-nowrap"
                  >
                    {item}
                  </span>
                ))}
              </span>
            </div>
          ))
        )}
        {preset.dedupe_enabled && (
          <div className="flex items-baseline gap-2">
            <span className="hud-label w-[84px] flex-shrink-0">Dedupe</span>
            <span className="text-[10.5px] font-mono text-ink-3">
              {preset.dedupe_duration_minutes == null
                ? "once per token, forever"
                : `fires again after ${preset.dedupe_duration_minutes}m`}
            </span>
          </div>
        )}
      </div>

      {/* execution + assignment summary */}
      <div className="mt-3 pt-2.5 border-t border-line/10 flex items-center gap-3 flex-wrap">
        <span className="hud-label">Exec</span>
        {autotrade ? (
          <>
            {execLine && (
              <span className="text-[11.5px] font-mono text-ink-1">{execLine}</span>
            )}
            {ladderLine && (
              <span
                className="text-[10.5px] font-mono text-ink-3"
                title="Take-profit / stop-loss ladder"
              >
                {ladderLine}
              </span>
            )}
          </>
        ) : (
          <span className="text-[11.5px] font-mono text-ink-4">
            none (set a buy size to start auto-buying)
          </span>
        )}
        <span className="ml-auto flex items-center gap-1.5 flex-wrap">
          <span className="hud-label">Bots</span>
          {assignments.length === 0 ? (
            <span className="text-[11.5px] font-mono text-ink-4">none assigned</span>
          ) : (
            assignments.map((a) => (
              <span
                key={a.client.id}
                className={`inline-flex items-center gap-1 px-1.5 h-[18px] rounded-sm border text-[10.5px] font-mono whitespace-nowrap ${
                  a.row.enabled
                    ? "border-accent/40 text-accent bg-accent/5"
                    : "border-line/40 text-ink-4 bg-card/60 line-through"
                }`}
                title={`${a.client.address}${a.row.enabled ? "" : " (assignment paused)"}${
                  a.row.buy_size_lamports_override != null
                    ? ` · buy override ${fmtSol(a.row.buy_size_lamports_override)} SOL`
                    : ""
                }${a.row.ladder_override ? " · ladder override" : ""}`}
              >
                {a.client.label?.trim() || shortAddr(a.client.address)}
                {(a.row.buy_size_lamports_override != null || a.row.ladder_override) && (
                  <span className="text-ink-4">*</span>
                )}
              </span>
            ))
          )}
        </span>
      </div>
    </div>
  );
}
