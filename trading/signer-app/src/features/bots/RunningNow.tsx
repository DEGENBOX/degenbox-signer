// Running now — the first thing on the Solana BOTS tab (operator R4:
// "I can't tell which presets and copy wallets are actually live").
// One compact row per LIVE thing: every enabled auto-buy session
// (preset, wallet, spent/budget, fills, armed-here vs server-only) and
// every enabled copy follow (leader, sizing, exit style, feed health).
// Each row has a status dot and a Manage jump to where it's edited;
// everything below this section is configuration.

import { Moon, Zap } from "lucide-react";
import { StatusPill, fmtSol, shortAddr } from "@degenbox/ui";
import { EmptyState, timeAgo } from "../../components/ui";
import type { CopytradeConfig, SolCopyConfigFull } from "../../ipc";
import type { BotDeviceStatus, BotPreset, ClientInfo } from "./ipc";
import { fmtIn } from "./meta";
import { sellSummary, sizingSummary } from "../presets/ipc";

/** Anchor ids the Manage buttons jump to (set in SolBotsTab/ClientCard). */
export const WALLETS_ANCHOR = "sol-wallets";
export const COPY_ANCHOR = "sol-copytrade";
export const walletAnchor = (address: string) => `sol-wallet-${address}`;

/** Fired before a wallet jump so the Bots accordion can expand the
 * target wallet (its body is collapsed by default). Detail = wallet
 * address, or null for the generic "wallets" anchor. */
export const WALLET_JUMP_EVENT = "dbx:wallet-jump";

/** Fired when a Manage jump needs the parent tab to switch its config
 * sub-nav to the section that owns `anchor` (R5 sub-nav). Detail =
 * `{ sub: "wallets" | "copy", anchor }`. The parent switches the sub-tab
 * and — after the target section has mounted — scrolls to `anchor`, so the
 * jump works even when that section was on an inactive sub-tab. */
export const BOTS_SUBTAB_EVENT = "dbx:bots-subtab";

function jumpTo(id: string, walletAddress?: string | null) {
  // Ask the accordion to open the target wallet first, so the jump lands
  // on an expanded card instead of a collapsed header.
  window.dispatchEvent(
    new CustomEvent(WALLET_JUMP_EVENT, { detail: walletAddress ?? null }),
  );
  // Tell the parent which config sub-tab owns this anchor. The parent
  // switches to it and does the scroll AFTER the section mounts (a bare
  // rAF-scroll here would race React's commit and land on nothing).
  const sub = id === COPY_ANCHOR ? "copy" : "wallets";
  window.dispatchEvent(
    new CustomEvent(BOTS_SUBTAB_EVENT, { detail: { sub, anchor: id } }),
  );
}

export function RunningNow({
  sessions,
  clients,
  device,
  copyRows,
  copyStats,
}: {
  sessions: BotPreset[] | null;
  clients: ClientInfo[] | null;
  device: BotDeviceStatus | null;
  copyRows: SolCopyConfigFull[] | null;
  copyStats: CopytradeConfig[] | null;
}) {
  const loading = sessions === null && copyRows === null;
  const armedIds = new Set(device?.armed_session_ids ?? []);
  const live = (sessions ?? []).filter((s) => s.enabled);
  const follows = (copyRows ?? []).filter((c) => c.enabled);

  const walletName = (pubkey: string | null): string => {
    if (!pubkey) return "unknown wallet";
    const c = clients?.find((x) => x.address === pubkey);
    return c ? c.label?.trim() || shortAddr(c.address, 4, 4) : shortAddr(pubkey, 4, 4);
  };

  return (
    <div className="card">
      {loading ? (
        <div style={{ display: "grid", gap: 8 }} aria-busy>
          <span className="skeleton" style={{ width: "55%" }} />
          <span className="skeleton" style={{ width: "40%" }} />
        </div>
      ) : live.length + follows.length === 0 ? (
        <EmptyState
          icon={<Moon size={18} />}
          title="Nothing is running right now"
          hint="start a session on a wallet below, or switch on a leader follow under Copy trade"
        />
      ) : (
        <>
          {live.length > 0 && device && armedIds.size === 0 && (
            <div className="banner warn" role="alert" style={{ marginBottom: 8 }}>
              <Zap size={15} style={{ flexShrink: 0 }} />
              <span style={{ flex: 1 }}>
                {device.unlocked
                  ? "These sessions are live on the server, but nothing is armed on THIS device, so no auto-buy fires from here. Jump to a wallet below and arm it."
                  : "The signer is locked. Unlock it to arm sessions; until then no auto-buy fires from this device."}
              </span>
            </div>
          )}
          <div className="run-list">
            {live.map((s) => {
              const armedHere = armedIds.has(s.id);
              const knownWallet =
                s.wallet_pubkey != null &&
                (clients ?? []).some((c) => c.address === s.wallet_pubkey);
              const meta = [
                `${fmtSol(s.spent_lamports)} / ${fmtSol(s.budget_lamports)} SOL`,
                `${s.fill_count} Fill${s.fill_count === 1 ? "" : "s"}`,
                s.expires_at
                  ? new Date(s.expires_at) > new Date()
                    ? `ends in ${fmtIn(s.expires_at)}`
                    : "past its end time"
                  : null,
              ].filter(Boolean);
              return (
                <div className="run-row" key={s.id}>
                  <span
                    className={`status-dot ${armedHere ? "green pulse" : "amber"}`}
                  />
                  <span className="run-kind hud-label">auto-buy</span>
                  <span className="run-main">
                    <strong>{s.name}</strong>
                    <span className="run-sub">
                      on {walletName(s.wallet_pubkey)} · {fmtSol(s.per_trade_lamports)}{" "}
                      SOL per buy
                    </span>
                  </span>
                  <span className="run-meta mono" title="spent / budget · fills · expiry">
                    {meta.join(" · ")}
                  </span>
                  {device &&
                    (armedHere ? (
                      <StatusPill
                        tone="ok"
                        icon={Zap}
                        title="This device's engine is armed. Auto-buys fire from here."
                      >
                        this device
                      </StatusPill>
                    ) : (
                      <StatusPill
                        tone="warn"
                        title="Live on the server but NOT armed on this device, so no auto-buy fires from here. Manage the session below to arm it."
                      >
                        not armed here
                      </StatusPill>
                    ))}
                  <button
                    className="btn xs"
                    title="Jump to this session's wallet below (arm, stop, budget)"
                    onClick={() =>
                      jumpTo(
                        knownWallet ? walletAnchor(s.wallet_pubkey!) : WALLETS_ANCHOR,
                        knownWallet ? s.wallet_pubkey : null,
                      )
                    }
                  >
                    Manage
                  </button>
                </div>
              );
            })}
            {follows.map((c) => {
              const st =
                copyStats?.find((x) => x.id === c.id && x.venue === "solana") ?? null;
              const statLine = st
                ? st.last_copy_at
                  ? `${st.copied_24h} cop${st.copied_24h === 1 ? "y" : "ies"} 24h · last ${timeAgo(st.last_copy_at)}`
                  : "no copies yet"
                : null;
              return (
                <div className="run-row" key={c.id}>
                  <span
                    className={`status-dot ${c.wallet_copy_mode ? "green pulse" : "amber"}`}
                  />
                  <span className="run-kind hud-label">copy</span>
                  <span className="run-main">
                    <strong>{c.label}</strong>
                    <span className="run-sub" title={c.leader}>
                      {shortAddr(c.leader, 6, 6)} · {sizingSummary(c)} ·{" "}
                      {sellSummary(c)}
                    </span>
                  </span>
                  {statLine && <span className="run-meta mono">{statLine}</span>}
                  {!c.wallet_copy_mode && (
                    <StatusPill
                      tone="warn"
                      title="This wallet's copy feed is off, so nothing gets mirrored. Open its settings under Copy trade and save to fix it."
                    >
                      feed off
                    </StatusPill>
                  )}
                  <button
                    className="btn xs"
                    title="Jump to Copy trade below (settings, stop following)"
                    onClick={() => jumpTo(COPY_ANCHOR)}
                  >
                    Manage
                  </button>
                </div>
              );
            })}
          </div>
        </>
      )}
    </div>
  );
}
