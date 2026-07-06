// Dense per-client session table — the arm/disarm machinery from the
// old Bots page, scoped to one client (sessions belong to clients via
// their wallet_pubkey). Stop disarms the local engine FIRST, then
// cancels the server row — preserved contract.

import { Moon, Power, Zap } from "lucide-react";
import { StatusPill, fmtSol } from "@degenbox/ui";
import { EmptyState, SkeletonRows, timeAgo } from "../../components/ui";
import { fmtIn } from "./meta";
import type { BotPreset } from "./ipc";

export interface SessionListProps {
  sessions: BotPreset[];
  /** Session ids this device's engine is armed for (daemon truth). */
  armedIds: Set<string>;
  /** Signer unlocked — arming requires it. */
  unlocked: boolean;
  /** True while ANY session mutation is in flight. */
  busy: boolean;
  /** Session wallet ≠ primary executor — arm fires from the primary
   * wallet, so flag it (the engine signs with the :5829 slot). */
  walletIsPrimary: boolean;
  loading?: boolean;
  onArm: (s: BotPreset) => void;
  onStop: (s: BotPreset) => void;
}

export function SessionList({
  sessions,
  armedIds,
  unlocked,
  busy,
  walletIsPrimary,
  loading,
  onArm,
  onStop,
}: SessionListProps) {
  return (
    <table className="table">
      <thead>
        <tr>
          <th>Preset</th>
          <th>Status</th>
          <th className="num">Buy</th>
          <th>TP ladder</th>
          <th className="num">SL</th>
          <th className="num">Budget</th>
          <th className="num">Fills</th>
          <th className="num">Expires</th>
          <th />
        </tr>
      </thead>
      <tbody>
        {loading ? (
          <SkeletonRows rows={1} cols={9} />
        ) : sessions.length === 0 ? (
          <tr>
            <td colSpan={9}>
              <EmptyState
                icon={<Moon size={16} />}
                title="No sessions on this client yet"
                hint="start one to auto-buy a preset's signals"
              />
            </td>
          </tr>
        ) : (
          sessions.map((s) => {
            const armedHere = armedIds.has(s.id);
            return (
              <tr key={s.id}>
                <td>
                  <strong>{s.name}</strong>
                </td>
                <td style={{ whiteSpace: "nowrap" }}>
                  <span style={{ display: "inline-flex", gap: 5, alignItems: "center" }}>
                    <StatusPill tone={s.enabled ? "ok" : "muted"} dot={s.enabled}>
                      {s.enabled ? "running" : "ended"}
                    </StatusPill>
                    {s.enabled &&
                      (armedHere ? (
                        <StatusPill
                          tone="ok"
                          icon={Zap}
                          title="This device's engine is armed. Auto-buys fire from here."
                        >
                          device
                        </StatusPill>
                      ) : (
                        <StatusPill
                          tone="warn"
                          title="Server row only. This device is NOT armed for it; arm to trade from here."
                        >
                          not armed
                        </StatusPill>
                      ))}
                  </span>
                </td>
                <td className="num">{s.buy_sol} SOL</td>
                <td>
                  {s.tp_ladder.length === 0 ? (
                    <span style={{ color: "var(--fg-faint)" }}>none</span>
                  ) : (
                    <span className="mono" style={{ fontSize: 11 }}>
                      {s.tp_ladder.map((l) => `${l.pct}%@${l.multiple}x`).join(" · ")}
                    </span>
                  )}
                </td>
                <td className="num">{s.sl_pct != null ? `-${s.sl_pct}%` : "—"}</td>
                <td className="num">
                  <span className="mono" style={{ fontSize: 11 }}>
                    {fmtSol(s.spent_lamports)} / {fmtSol(s.budget_lamports)}
                  </span>
                </td>
                <td className="num">{s.fill_count}</td>
                <td className="num">
                  {s.expires_at
                    ? new Date(s.expires_at) > new Date()
                      ? `in ${fmtIn(s.expires_at)}`
                      : timeAgo(s.expires_at)
                    : "—"}
                </td>
                <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
                  {s.enabled && !armedHere && (
                    <button
                      className="btn xs"
                      style={{ marginRight: 4 }}
                      disabled={busy || !unlocked}
                      title={
                        !unlocked
                          ? "Unlock the signer first"
                          : walletIsPrimary
                            ? "Arm this device's engine for the session (replaces any currently-armed session)"
                            : "Arming executes via the primary executor wallet. This session's wallet is not primary"
                      }
                      onClick={() => onArm(s)}
                    >
                      <Zap size={12} /> Arm
                    </button>
                  )}
                  {s.enabled && (
                    <button
                      className="btn xs danger"
                      disabled={busy}
                      title="Disarm this device, then cancel the server session"
                      onClick={() => onStop(s)}
                    >
                      <Power size={12} /> Stop
                    </button>
                  )}
                </td>
              </tr>
            );
          })
        )}
      </tbody>
    </table>
  );
}
