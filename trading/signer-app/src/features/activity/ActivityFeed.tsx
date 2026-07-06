// Live Bot-activity feed — the desktop twin of the web Bot tabs
// (BotPanel / SolBotPanel). One row per recent instruction the bot
// queued, with its live signer + exchange/on-chain lifecycle, the caller
// or copied wallet behind it, and the failure reason on any miss. This
// is the operator's bar for the signer's own screen: "show what the bot
// is doing, and why, live."
//
// A single presentational shell (<FeedShell>) renders a venue-agnostic
// row view-model; the two exported wrappers own the poll + filter state
// and adapt the HL / Solana rows through the shared lifecycle helpers.

import { useMemo, useState, type ComponentType } from "react";
import {
  ArrowDownRight,
  ArrowUpRight,
  Ban,
  Copy,
  CornerDownLeft,
  Gauge,
  Inbox,
  Layers,
  RefreshCw,
  Shield,
  Zap,
} from "lucide-react";
import { fmtUsd, shortAddr, timeAgo } from "../../components/ui";
import { useHlBotActivity, useSolBotActivity } from "./useBotActivity";
import {
  hlKindLabel,
  hlLifecycle,
  solLifecycle,
  solReasonLabel,
  solSourceLabel,
  solTokenLabel,
  splitMarket,
  type HlActivityRow,
  type LifecycleStage,
  type SolActivityRow,
  type Tone,
} from "./lifecycle";
import "./activity.css";

type LucideIcon = ComponentType<{ size?: number | string }>;

/** Venue-agnostic row the shell knows how to render. */
interface FeedVM {
  key: string;
  createdAt: string;
  actionIcon: LucideIcon;
  actionLabel: string;
  actionTone: "buy" | "sell" | "neutral";
  asset: { symbol: string; dex: string | null } | null;
  size: string;
  lev: string | null;
  source: { text: string; manual: boolean; title?: string };
  stage: LifecycleStage;
  detail: React.ReactNode;
}

const TONE_CLASS: Record<Tone, string> = {
  good: "tone-good",
  bad: "tone-bad",
  warn: "tone-warn",
  info: "tone-info",
  muted: "tone-muted",
};

function fmtSol(lamports: number): string {
  const sol = lamports / 1e9;
  return sol >= 1 ? sol.toFixed(2) : sol.toFixed(4);
}

// ─── presentational shell ─────────────────────────────────────────

interface Filter {
  key: string;
  label: string;
}

function FeedShell({
  rows,
  filters,
  active,
  onFilter,
  loading,
  error,
  onRetry,
  live,
  empty,
}: {
  rows: FeedVM[] | null;
  filters: Filter[];
  active: string;
  onFilter: (k: string) => void;
  loading: boolean;
  error: string | null;
  onRetry: () => void;
  live: boolean;
  empty: { title: string; sub: string };
}) {
  const count = rows?.length ?? 0;
  return (
    <div className="act-feed">
      <div className="act-filters">
        <span className="act-lead">
          <Layers size={13} />
        </span>
        {filters.map((f) => (
          <button
            key={f.key}
            className={`act-pill ${active === f.key ? "active" : ""}`}
            onClick={() => onFilter(f.key)}
          >
            {f.label}
          </button>
        ))}
        <span className="act-count">
          {count} row{count === 1 ? "" : "s"}
          <span
            className={`status-dot ${live && !error ? "green pulse" : error ? "red" : "amber"}`}
            style={{ marginLeft: 8, verticalAlign: "middle" }}
            title={
              error
                ? "feed unavailable"
                : live
                  ? "live (polls every 4s)"
                  : "reconnecting…"
            }
          />
        </span>
      </div>

      {error ? (
        <div className="act-error" role="alert">
          <span style={{ flex: 1 }}>
            Couldn't reach the gateway for bot activity: {error}. Execution
            keeps running; this is just the read feed.
          </span>
          <button className="btn act-fix" onClick={onRetry}>
            <RefreshCw size={13} /> Retry
          </button>
        </div>
      ) : rows === null || loading ? (
        <div className="act-state">Opening the activity feed…</div>
      ) : rows.length === 0 ? (
        <div className="act-empty">
          <span className="act-empty-icon">
            <Inbox size={26} strokeWidth={1.5} />
          </span>
          <span className="act-empty-title">{empty.title}</span>
          <span className="act-empty-sub">{empty.sub}</span>
        </div>
      ) : (
        <div className="act-scroll">
          <table className="act-table">
            <thead>
              <tr>
                <th>Time</th>
                <th>Action</th>
                <th>Asset</th>
                <th className="r">Size</th>
                <th>From</th>
                <th>Lifecycle</th>
                <th>Detail</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <FeedRow key={r.key} vm={r} />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

function FeedRow({ vm }: { vm: FeedVM }) {
  const Icon = vm.actionIcon;
  const toneClass = TONE_CLASS[vm.stage.tone];
  const bad = vm.stage.tone === "bad";
  return (
    <tr>
      <td className="act-time" title={new Date(vm.createdAt).toLocaleString()}>
        {timeAgo(vm.createdAt)}
      </td>
      <td>
        <span className={`act-action ${vm.actionTone}`}>
          <Icon size={13} />
          {vm.actionLabel}
        </span>
      </td>
      <td>
        {vm.asset ? (
          <span className="act-asset">
            {vm.asset.symbol}
            {vm.asset.dex && (
              <span className="act-dex" title="Builder-DEX market">
                {vm.asset.dex}
              </span>
            )}
          </span>
        ) : (
          <span className="act-dim">—</span>
        )}
      </td>
      <td className="r act-size">
        {vm.size}
        {vm.lev && <span className="act-lev">{vm.lev}</span>}
      </td>
      <td>
        <span
          className={`act-source ${vm.source.manual ? "manual" : ""}`}
          title={vm.source.title}
        >
          {vm.source.text}
        </span>
      </td>
      <td>
        <span className="act-life">
          <span className={`act-bar ${toneClass}`} aria-hidden>
            {[1, 2, 3].map((seg) => {
              const on = bad ? seg === 1 : seg <= vm.stage.reached;
              return <span key={seg} className={`seg ${on ? "on" : ""}`} />;
            })}
          </span>
          <span className={`act-chip ${toneClass}`} title={vm.stage.detail}>
            {!vm.stage.terminal && (
              <span className="act-live">
                <span className="ping" />
                <span />
              </span>
            )}
            {vm.stage.label}
          </span>
        </span>
      </td>
      <td className="act-detail">{vm.detail}</td>
    </tr>
  );
}

// ─── Perpetuals wrapper ───────────────────────────────────────────

type HlFilter = "all" | "opens" | "exits" | "stops" | "cancel";
const HL_FILTERS: Filter[] = [
  { key: "all", label: "All" },
  { key: "opens", label: "Opens" },
  { key: "exits", label: "Closes" },
  { key: "stops", label: "SL / TP" },
  { key: "cancel", label: "Cancels" },
];

function passesHl(kind: HlActivityRow["kind"], f: HlFilter): boolean {
  switch (f) {
    case "all":
      return true;
    case "opens":
      return kind === "entry" || kind === "leverage";
    case "exits":
      return kind === "close";
    case "stops":
      return kind === "sl" || kind === "tp";
    case "cancel":
      return kind === "cancel";
  }
}

function hlToVM(row: HlActivityRow): FeedVM {
  const stage = hlLifecycle(row);
  const isExit = row.kind === "close" || row.reduce_only === true;
  const actionIcon: LucideIcon =
    row.kind === "cancel"
      ? Ban
      : row.kind === "leverage"
        ? Gauge
        : row.kind === "sl" || row.kind === "tp"
          ? Shield
          : isExit
            ? CornerDownLeft
            : row.side === "buy"
              ? ArrowUpRight
              : ArrowDownRight;
  const actionTone: FeedVM["actionTone"] =
    row.kind === "sl" ||
    row.kind === "tp" ||
    row.kind === "cancel" ||
    row.kind === "leverage" ||
    isExit
      ? "neutral"
      : row.side === "buy"
        ? "buy"
        : "sell";

  const size = row.size_usd != null ? Number(row.size_usd) : null;
  const filled = row.filled_size_usd != null ? Number(row.filled_size_usd) : null;
  const pnl = row.closed_pnl != null ? Number(row.closed_pnl) : null;
  const caller = row.caller_name ?? row.caller_id ?? null;

  let detail: React.ReactNode = <span className="act-dim">—</span>;
  if (row.err_msg) {
    detail = (
      <span className="act-fail" title={row.err_msg}>
        {row.err_msg}
      </span>
    );
  } else if (pnl != null && pnl !== 0) {
    detail = (
      <span className={pnl >= 0 ? "act-gain" : "act-loss"}>
        {pnl >= 0 ? "+" : "−"}
        {fmtUsd(String(Math.abs(pnl)))} PnL
      </span>
    );
  } else if (filled != null && size != null && filled < size * 0.999) {
    detail = <span className="act-warn">filled {fmtUsd(String(filled))}</span>;
  }

  return {
    key: row.cloid,
    createdAt: row.created_at,
    actionIcon,
    actionLabel: hlKindLabel(row.kind),
    actionTone,
    asset: row.coin ? splitMarket(row.coin) : null,
    size: size != null ? fmtUsd(String(size)) : "—",
    lev: row.leverage ? `${row.leverage}×` : null,
    source: caller
      ? { text: caller, manual: false, title: row.signal_id ? `signal ${row.signal_id}` : undefined }
      : row.target_wallet
        ? { text: `copy ${shortAddr(row.target_wallet, 4, 4)}`, manual: false, title: "copy-trade follow" }
        : { text: "manual", manual: true },
    stage,
    detail,
  };
}

export function PerpsActivityFeed() {
  const { rows, error, live, refetch } = useHlBotActivity();
  const [filter, setFilter] = useState<HlFilter>("all");
  const vms = useMemo(
    () =>
      rows === null
        ? null
        : rows.filter((r) => passesHl(r.kind, filter)).map(hlToVM),
    [rows, filter],
  );
  return (
    <FeedShell
      rows={vms}
      filters={HL_FILTERS}
      active={filter}
      onFilter={(k) => setFilter(k as HlFilter)}
      loading={false}
      error={error}
      onRetry={refetch}
      live={live}
      empty={{
        title: "Your bot hasn't acted yet",
        sub: "When a caller you follow posts a trade, or a copied wallet fills, every instruction your bot queues appears here with its live signer + exchange status.",
      }}
    />
  );
}

// ─── Solana wrapper ───────────────────────────────────────────────

type SolFilter = "all" | "buys" | "sells" | "skipped";
const SOL_FILTERS: Filter[] = [
  { key: "all", label: "All" },
  { key: "buys", label: "Buys" },
  { key: "sells", label: "Sells" },
  { key: "skipped", label: "Skipped" },
];

function passesSol(row: SolActivityRow, f: SolFilter): boolean {
  switch (f) {
    case "all":
      return true;
    case "buys":
      return row.kind === "intent" && row.side === "buy";
    case "sells":
      return row.kind === "intent" && row.side === "sell";
    case "skipped":
      return row.kind === "skip";
  }
}

function solToVM(row: SolActivityRow): FeedVM {
  const stage = solLifecycle(row);
  const isSkip = row.kind === "skip";
  const actionIcon: LucideIcon = isSkip
    ? Ban
    : row.source === "copytrade"
      ? Copy
      : row.side === "buy"
        ? ArrowUpRight
        : row.side === "sell"
          ? ArrowDownRight
          : Zap;
  const actionTone: FeedVM["actionTone"] = isSkip
    ? "neutral"
    : row.side === "buy"
      ? "buy"
      : row.side === "sell"
        ? "sell"
        : "neutral";
  const actionLabel = isSkip
    ? "Skipped"
    : row.side === "buy"
      ? "Buy"
      : row.side === "sell"
        ? "Sell"
        : "Order";

  const lamports = row.amount_in_lamports;
  const size =
    lamports != null
      ? row.side === "buy"
        ? `${fmtSol(lamports)} SOL`
        : fmtSol(lamports)
      : "—";

  let detail: React.ReactNode = <span className="act-dim">—</span>;
  if (isSkip) {
    detail = (
      <span className="act-dim" title={row.reason ?? undefined}>
        {solReasonLabel(row.reason)}
      </span>
    );
  } else if (row.reason) {
    detail = (
      <span className="act-fail" title={row.reason}>
        {row.reason}
      </span>
    );
  } else if (row.signature) {
    detail = (
      <a
        href={`https://solscan.io/tx/${row.signature}`}
        target="_blank"
        rel="noreferrer"
      >
        {shortAddr(row.signature, 6, 6)}
      </a>
    );
  }

  const sym = solTokenLabel(row);
  return {
    key: `${row.kind}:${row.id}`,
    createdAt: row.created_at,
    actionIcon,
    actionLabel,
    actionTone,
    asset: sym === "—" ? null : { symbol: sym, dex: null },
    size,
    lev: null,
    source: {
      text: solSourceLabel(row),
      manual: row.source === "manual",
      title: row.target_wallet ?? undefined,
    },
    stage,
    detail,
  };
}

export function SolActivityFeed() {
  const { rows, error, live, refetch } = useSolBotActivity();
  const [filter, setFilter] = useState<SolFilter>("all");
  const vms = useMemo(
    () =>
      rows === null
        ? null
        : rows.filter((r) => passesSol(r, filter)).map(solToVM),
    [rows, filter],
  );
  return (
    <FeedShell
      rows={vms}
      filters={SOL_FILTERS}
      active={filter}
      onFilter={(k) => setFilter(k as SolFilter)}
      loading={false}
      error={error}
      onRetry={refetch}
      live={live}
      empty={{
        title: "Your bot hasn't acted yet",
        sub: "When a preset you armed fires, or a copied wallet buys, every trade your bot queues (and every one it skips) shows up here with its live signer + on-chain status.",
      }}
    />
  );
}
