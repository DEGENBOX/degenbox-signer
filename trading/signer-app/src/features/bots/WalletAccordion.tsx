// WalletAccordion — collapses each Solana wallet's card into an
// accordion (operator R4.7: "split section 02 into sub-segments,
// schlauer aufräumen"). The header is a compact identity strip
// (index · label · status dot · lead balance) that stays glanceable
// when collapsed; the body is the FULL existing ClientCard (identity,
// stats, SessionList, all actions) — reused unchanged so no
// functionality is lost.
//
// Single-open: the parent owns `expandedWallet`; opening one wallet
// closes the others. The body is kept MOUNTED and hidden with the
// `hidden` attribute (not conditionally rendered) so SessionList never
// remounts on expand/collapse — the exact remount-jitter the sweep
// flagged (ui-sweep-bot-findings-2026-07-07 §8).

import type { ReactNode } from "react";
import { ChevronDown } from "lucide-react";
import { isRemote, runtimeMeta } from "./meta";
import type { ClientInfo, SolWalletBalance } from "./ipc";

export function WalletAccordion({
  c,
  index,
  balance,
  sessionCount,
  open,
  onToggle,
  children,
}: {
  c: ClientInfo;
  index: number;
  balance: SolWalletBalance | null;
  /** Enabled-session count for the collapsed hint. */
  sessionCount: number | null;
  open: boolean;
  onToggle: () => void;
  /** The full ClientCard, rendered into the (always-mounted) body. */
  children: ReactNode;
}) {
  const meta = runtimeMeta(c);
  const remote = isRemote(c);
  const label = c.label?.trim() || (remote ? "Unnamed" : "Unnamed (rename)");

  return (
    // The id is the Running-now "Manage" jump target; it lives on the
    // always-visible wrapper (the card body may be collapsed).
    <section
      className="wallet-acc"
      id={c.address ? `sol-wallet-${c.address}` : undefined}
      aria-label={c.label ?? c.address}
    >
      <button
        className="wallet-acc-head"
        aria-expanded={open}
        onClick={onToggle}
        title={open ? "Collapse this wallet" : "Expand this wallet"}
      >
        <ChevronDown size={14} className={`collapsible-chev ${open ? "open" : ""}`} />
        <span className="section-num">{String(index + 1).padStart(2, "0")}</span>
        <span
          className={`status-dot ${meta.dot} ${meta.pulse ? "pulse" : ""}`}
          style={{ flexShrink: 0 }}
        />
        <span className="wallet-acc-label">{label}</span>
        {c.primary && (
          <span className="badge accent" title="Primary executor">
            primary
          </span>
        )}
        {remote && (
          <span className="badge" title="Registered on the gateway only">
            remote
          </span>
        )}
        {!remote && c.drift && (
          <span className="badge warn" title={c.drift}>
            drift
          </span>
        )}
        {/* lead stat: balance only, right-aligned; full stats live in the body */}
        <span className="wallet-acc-lead mono" title="Wallet SOL balance">
          {balance ? `${Number(balance.sol_ui)} SOL` : "—"}
        </span>
        {sessionCount != null && (
          <span
            className="hud-label brackets"
            title="Enabled auto-buy sessions on this wallet"
            style={{ flexShrink: 0 }}
          >
            {sessionCount}
          </span>
        )}
      </button>
      {/* Body stays mounted; hidden (display:none) when collapsed so the
          SessionList inside never remounts on toggle. */}
      <div className="wallet-acc-body" hidden={!open}>
        {children}
      </div>
    </section>
  );
}
