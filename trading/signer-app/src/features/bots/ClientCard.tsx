// One hairline client card — identity (inline-renamable label, mono
// address), runtime hud-line, balance/uPnL/positions/budget stat row,
// and the client's own bot sessions underneath. Fleet-table logic from
// Home/ClientTable, restructured per-client and reskinned (corners,
// hud-labels, dense mono).

import type { ReactNode } from "react";
import { Download, KeyRound, Pencil, Plus, Star, Trash2 } from "lucide-react";
import { fmtSol, fmtUsd } from "@degenbox/ui";
import {
  CopyButton,
  InlineEdit,
  RowMenu,
  Switch,
  Ticker,
  type MenuEntry,
} from "../../components/ui";
import { isRemote, num, runtimeMeta } from "./meta";
import { SessionList } from "./SessionList";
import type { BotPreset, ClientInfo, SolWalletBalance } from "./ipc";

export interface ClientCardProps {
  c: ClientInfo;
  index: number;
  balance: SolWalletBalance | null;
  sessions: BotPreset[];
  sessionsLoading: boolean;
  armedIds: Set<string>;
  unlocked: boolean;
  busy: boolean;
  sessionBusy: boolean;
  onToggleActive: (c: ClientInfo, active: boolean) => void;
  onRename: (c: ClientInfo, label: string) => void;
  onSetPrimary: (c: ClientInfo) => void;
  onActivate: (c: ClientInfo) => void;
  onExport: (c: ClientInfo) => void;
  onRemove: (c: ClientInfo) => void;
  onBudget: (c: ClientInfo) => void;
  onStartSession: (c: ClientInfo) => void;
  onArm: (s: BotPreset) => void;
  onStop: (s: BotPreset) => void;
  /** When true the wrapper (WalletAccordion) owns the jump-anchor id, so
   * the card must not also emit it (duplicate ids are invalid). */
  anchorOnWrapper?: boolean;
}

export function ClientCard({
  c,
  index,
  balance,
  sessions,
  sessionsLoading,
  armedIds,
  unlocked,
  busy,
  sessionBusy,
  anchorOnWrapper,
  onToggleActive,
  onRename,
  onSetPrimary,
  onActivate,
  onExport,
  onRemove,
  onBudget,
  onStartSession,
  onArm,
  onStop,
}: ClientCardProps) {
  const meta = runtimeMeta(c);
  const remote = isRemote(c);
  const pnl = num(c.gateway?.unrealized_pnl_usd);
  const positions = c.gateway?.open_positions ?? null;
  const budget = c.gateway?.budget ?? null;
  const budgetParts = [
    budget?.session_budget_lamports != null
      ? `session ${fmtSol(budget.session_budget_lamports)}`
      : null,
    budget?.per_trade_lamports != null
      ? `trade ${fmtSol(budget.per_trade_lamports)}`
      : null,
  ].filter(Boolean) as string[];

  const menu: (MenuEntry | "sep")[] = remote
    ? []
    : [
        ...(!c.primary
          ? [
              {
                label: "Make primary executor",
                icon: <Star size={13} />,
                disabled: busy || !c.unlocked,
                hint: c.unlocked
                  ? "Route Solana execution through this wallet"
                  : "Activate / unlock this wallet first",
                onClick: () => onSetPrimary(c),
              } satisfies MenuEntry,
            ]
          : []),
        ...(!c.unlocked
          ? [
              {
                label: "Activate runtime…",
                icon: <KeyRound size={13} />,
                hint: "Bring this wallet's runtime online without re-locking the app",
                onClick: () => onActivate(c),
              } satisfies MenuEntry,
            ]
          : []),
        {
          label: "Export keystore…",
          icon: <Download size={13} />,
          onClick: () => onExport(c),
        },
        "sep",
        {
          label: "Remove from device…",
          icon: <Trash2 size={13} />,
          danger: true,
          onClick: () => onRemove(c),
        },
      ];

  const statusTitle = [meta.detail, c.drift ? `drift: ${c.drift}` : null]
    .filter(Boolean)
    .join(" · ");

  return (
    // The id is the Running-now "Manage" jump target (features/bots/RunningNow).
    // Inside the accordion the wrapper carries the id instead (it stays
    // visible while the body is collapsed), so we suppress it here.
    <section
      className="card"
      id={!anchorOnWrapper && c.address ? `sol-wallet-${c.address}` : undefined}
      aria-label={c.label ?? c.address}
    >
      {/* identity row */}
      <div style={{ display: "flex", alignItems: "flex-start", gap: 12 }}>
        <div style={{ flex: 1, minWidth: 0 }}>
          <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
            <span className="section-num">{String(index + 1).padStart(2, "0")}</span>
            {remote ? (
              <span
                style={{ fontSize: 14, fontWeight: 600, color: "var(--fg)" }}
                title="Registered on the gateway only (no key in this vault)"
              >
                {c.label ?? "Unnamed"}
              </span>
            ) : (
              <InlineEdit
                value={c.label}
                placeholder="Unnamed (rename)"
                busy={busy}
                onCommit={(label) => onRename(c, label)}
              />
            )}
            {c.primary && (
              <span
                className="badge accent"
                title="Primary executor. Also runs legacy (unstamped) work"
              >
                primary
              </span>
            )}
            {remote && (
              <span
                className="badge"
                title={
                  c.drift ??
                  "Remote: registered on your DegenBox account but running on another device (or as a gateway-side binding). This app holds no key for it."
                }
              >
                remote
              </span>
            )}
            {!remote && c.drift && (
              <span className="badge warn" title={c.drift}>
                drift
              </span>
            )}
          </div>
          <div
            className="mono"
            style={{
              fontSize: 11,
              color: "var(--fg-faint)",
              display: "flex",
              alignItems: "center",
              gap: 4,
              marginTop: 2,
              minWidth: 0,
            }}
          >
            <span
              style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}
            >
              {c.address || "no address"}
            </span>
            {c.address && <CopyButton text={c.address} label="Copy address" />}
          </div>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 10, flexShrink: 0 }}>
          <span
            className="cell-status hud-label"
            style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
            title={statusTitle || undefined}
          >
            <span className={`status-dot ${meta.dot} ${meta.pulse ? "pulse" : ""}`} />
            {meta.label}
          </span>
          {remote ? (
            <Switch
              on={!(c.gateway?.paused ?? false)}
              disabled
              title="Managed on the gateway (no key in this vault)"
              onToggle={() => {}}
            />
          ) : (
            <Switch
              on={!c.paused}
              disabled={busy}
              title={c.paused ? "Resume this client" : "Pause this client (queued work waits)"}
              onToggle={(next) => onToggleActive(c, next)}
            />
          )}
          {menu.length > 0 && <RowMenu entries={menu} />}
        </div>
      </div>

      {/* stat line — min-height keeps the row stable when balance/uPnL
          change digit-length on the 10s fleet poll. */}
      <div
        style={{
          display: "flex",
          flexWrap: "wrap",
          gap: "10px 26px",
          marginTop: 12,
          paddingTop: 10,
          borderTop: "1px solid rgb(var(--line) / 0.1)",
          minHeight: 42,
        }}
      >
        <CardStat label="Balance" hint="Wallet SOL balance">
          {balance ? (
            <Ticker value={Number(balance.sol_ui)} format={(n) => `${n} SOL`} animate={false} />
          ) : (
            <span style={{ color: "var(--fg-faint)" }} title="Balance not fetched yet">
              —
            </span>
          )}
        </CardStat>
        <CardStat label="uPnL" hint="Unrealized PnL across this wallet's open positions">
          <Ticker
            value={pnl}
            format={(n) => `${n > 0 ? "+" : ""}${fmtUsd(n)}`}
            className={pnl ? (pnl > 0 ? "pos" : "neg") : ""}
            animate={false}
          />
        </CardStat>
        <CardStat label="Positions" hint="Open positions on this wallet">
          {positions ?? "—"}
        </CardStat>
        <CardStat label="Budget">
          <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
            <span className="mono" style={{ fontSize: 12 }}>
              {c.gateway
                ? budgetParts.length > 0
                  ? `${budgetParts.join(" · ")} SOL`
                  : "uncapped"
                : "local only"}
            </span>
            {c.gateway && (
              <button
                className="btn icon"
                title="Edit this client's server-side caps (session + per-trade)"
                onClick={() => onBudget(c)}
              >
                <Pencil size={11} />
              </button>
            )}
          </span>
        </CardStat>
        {c.gateway?.last_activity && (
          <CardStat label="Last activity">
            <span className="mono" style={{ fontSize: 12 }}>
              {timeAgoShort(c.gateway.last_activity)}
            </span>
          </CardStat>
        )}
      </div>

      {/* sessions */}
      <div style={{ marginTop: 14 }}>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            justifyContent: "space-between",
            marginBottom: 6,
          }}
        >
          <span style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
            <span className="hud-label">Sessions</span>
            <span className="hud-label brackets">
              {sessionsLoading ? "…" : sessions.filter((s) => s.enabled).length}
            </span>
          </span>
          {!remote && (
            <button
              className="btn xs"
              title="Create a budgeted auto-buy session on this client"
              onClick={() => onStartSession(c)}
            >
              <Plus size={12} /> Session
            </button>
          )}
        </div>
        {/* min-height reserves space so the skeleton→rows→empty transitions
            don't shift the card on the 10s poll. */}
        <div style={{ minHeight: 96 }}>
          {sessions.length === 0 && !sessionsLoading && remote ? (
            <p className="empty" style={{ margin: 0, padding: "6px 0" }}>
              No sessions on this wallet.
            </p>
          ) : (
            <SessionList
              sessions={sessions}
              armedIds={armedIds}
              unlocked={unlocked}
              busy={sessionBusy}
              walletIsPrimary={c.primary}
              loading={sessionsLoading}
              onArm={onArm}
              onStop={onStop}
            />
          )}
        </div>
      </div>
    </section>
  );
}

function CardStat({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    // min-width stops a value's digit-length change from reflowing the row.
    <div style={{ display: "grid", gap: 2, minWidth: 85 }}>
      <span className="hud-label" title={hint}>
        {label}
      </span>
      <span style={{ fontSize: 13, color: "var(--fg)", fontVariantNumeric: "tabular-nums" }}>
        {children}
      </span>
    </div>
  );
}

/** "4m" style — local copy so the card has no clientMeta dependency. */
function timeAgoShort(iso: string): string {
  const ms = Date.now() - new Date(iso).getTime();
  if (!Number.isFinite(ms) || ms < 0) return "—";
  const m = Math.floor(ms / 60_000);
  if (m < 1) return "now";
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.floor(h / 24)}d`;
}
