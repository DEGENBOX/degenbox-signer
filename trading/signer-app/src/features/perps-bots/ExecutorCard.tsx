// The single Perpetuals "bot card" — ClientCard idiom (hairline card,
// identity row, hud stat line, sub-blocks) applied to this device's
// HL executor. There is no fleet on the perps side: this ONE card is
// the bot, so it carries the page's one corner-bracket signature
// (calm-pass decoration budget, docs/ui-idiom.md). Identity = the
// master account it trades for; runtime = the HL daemon conn; the
// Switch is the device-wide signing kill-switch (there is no HL-only
// pause command — documented gap).

import { useState, type ReactNode } from "react";
import {
  ArrowRightLeft,
  FlaskConical,
  KeyRound,
  Link2,
  Settings2,
  Trash2,
  Unlink,
} from "lucide-react";
import { StatusPill, fmtUsdOrDash } from "@degenbox/ui";
import {
  CopyButton,
  RowMenu,
  Switch,
  Ticker,
  shortAddr,
  timeAgo,
  type MenuEntry,
} from "../../components/ui";
import { PAIRING_PILL, connMeta, pairingHealthy } from "./meta";
import { SpotPerpTransferDialog } from "./SpotPerpTransferDialog";
import type { HlPairingStatus, HlStatus, StatusReport } from "./ipc";

export interface ExecutorCardProps {
  status: StatusReport;
  hl: HlStatus;
  /** Server-side pairing truth (null = no token yet / old gateway). */
  pairing: HlPairingStatus | null;
  pairingErr: string | null;
  busy: boolean;
  /** Device-wide signing kill-switch (set_paused — both runtimes). */
  onToggleSigning: (next: boolean) => void;
  onPaperMode: (next: boolean) => void;
  onRerunSetup: () => void;
  onPair: () => void;
  onUnpair: () => void;
  onRemoveKey: () => void;
}

export function ExecutorCard({
  status,
  hl,
  pairing,
  pairingErr,
  busy,
  onToggleSigning,
  onPaperMode,
  onRerunSetup,
  onPair,
  onUnpair,
  onRemoveKey,
}: ExecutorCardProps) {
  const conn = connMeta(hl);
  const acct = Number(hl.balance.account_value_usd);
  const perpValue = Number.isFinite(acct) && hl.balance.account_value_usd != null ? acct : null;
  // SPOT USDC is a SEPARATE HL wallet from perp on a SEPARATED account.
  // `null` = fetch failed (render "—").
  const spot = Number(hl.balance.spot_usdc);
  const spotUsdc = Number.isFinite(spot) && hl.balance.spot_usdc != null ? spot : null;
  // UNIFIED account: HL trades ONE balance (spot backs perp automatically)
  // and greys out the spot↔perp transfer. Show a SINGLE truthful value and
  // hide the transfer entirely; perp accountValue alone reads $0 with the
  // money in spot. SEPARATED account: keep the co-equal perp + spot split
  // + the transfer (spot is a distinct wallet needing a move to trade).
  const isUnified = hl.balance.is_unified ?? false;
  const uni = Number(hl.balance.unified_value_usd);
  const unifiedValue =
    Number.isFinite(uni) && hl.balance.unified_value_usd != null ? uni : null;
  // The single balance figure the card leads with.
  const acctValue = isUnified ? unifiedValue : perpValue;
  const hasIdleSpot = spotUsdc != null && spotUsdc > 0.01;
  // "Money's in spot, move it to perp" only makes sense on a SEPARATED
  // account — a unified account trades off the spot USDC directly.
  const perpEmptySpotFunded = !isUnified && perpValue === 0 && hasIdleSpot;
  const [transferOpen, setTransferOpen] = useState(false);

  const menu: (MenuEntry | "sep")[] = [
    {
      label: `Paper mode ${hl.paper_mode ? "OFF" : "ON"}`,
      icon: <FlaskConical size={13} />,
      disabled: busy,
      hint: "Paper mode resolves and reports every instruction but never submits an order. Takes effect immediately, starting with the next order.",
      onClick: () => onPaperMode(!hl.paper_mode),
    },
    {
      label: "Re-run setup…",
      icon: <Settings2 size={13} />,
      disabled: busy,
      hint: "Re-import the agent key and/or re-pair with the gateway",
      onClick: onRerunSetup,
    },
    "sep",
    ...(hl.paired
      ? [
          {
            label: "Unpair…",
            icon: <Unlink size={13} />,
            danger: true,
            disabled: busy,
            onClick: onUnpair,
          } satisfies MenuEntry,
        ]
      : []),
    {
      label: "Remove agent key…",
      icon: <Trash2 size={13} />,
      danger: true,
      disabled: busy,
      onClick: onRemoveKey,
    },
  ];

  const serverPairing = pairing ? (PAIRING_PILL[pairing.state] ?? null) : null;

  return (
    <section className="card corners" aria-label="Perpetuals executor">
      {/* identity row */}
      <div style={{ display: "flex", alignItems: "flex-start", gap: 12 }}>
        <div style={{ flex: 1, minWidth: 0 }}>
          <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
            <span style={{ fontSize: 14, fontWeight: 600, color: "var(--fg)" }}>
              Perpetuals executor
            </span>
            <span
              className="badge accent"
              title="This device is the sole Perpetuals executor. Every order signs here"
            >
              this device
            </span>
            {hl.paper_mode && (
              <StatusPill
                tone="info"
                icon={FlaskConical}
                title="Paper mode: instructions resolve and report but never submit"
              >
                paper
              </StatusPill>
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
            title="Master account: the wallet this executor trades for"
          >
            <span
              style={{ overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}
            >
              {hl.account_address ?? "master account unknown (pair to bind one)"}
            </span>
            {hl.account_address && (
              <CopyButton text={hl.account_address} label="Copy master account" />
            )}
          </div>
        </div>

        <div style={{ display: "flex", alignItems: "center", gap: 10, flexShrink: 0 }}>
          <span
            className="cell-status hud-label"
            style={{ display: "inline-flex", alignItems: "center", gap: 6 }}
            title={hl.last_poll_at ? `last poll ${timeAgo(hl.last_poll_at)}` : undefined}
          >
            <span className={`status-dot ${conn.dot} ${conn.pulse ? "pulse" : ""}`} />
            {conn.label}
          </span>
          <Switch
            on={!status.paused}
            disabled={busy}
            title={
              status.paused
                ? "Resume signing on this device (device-wide, both modes)"
                : "Pause ALL signing on this device (device-wide, both modes); queued orders wait"
            }
            onToggle={(next) => onToggleSigning(next)}
          />
          <RowMenu entries={menu} />
        </div>
      </div>

      {/* stat line — min-height keeps the row from jittering when a value
          changes digit-length on the 2s poll. */}
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
        <CardStat
          label={isUnified ? "Balance" : "Perp equity"}
          hint={
            isUnified
              ? "Account value (unified — spot backs perp automatically)"
              : "Perp equity (spot shown separately)"
          }
        >
          {acctValue != null ? (
            <Ticker value={acctValue} format={(n) => fmtUsdOrDash(n)} />
          ) : (
            <Dash hint="Balance not fetched yet" />
          )}
        </CardStat>
        {/* Separated account only: spot is a distinct wallet. On a unified
            account the spot USDC is already inside "Balance", so a separate
            "Spot USDC" stat would double-count + mislead. */}
        {!isUnified && (
          <CardStat label="Spot USDC">
            {spotUsdc != null ? (
              <span style={{ color: hasIdleSpot ? "var(--amber)" : undefined }}>
                {fmtUsdOrDash(spotUsdc)}
              </span>
            ) : (
              <Dash hint="Spot balance not fetched yet" />
            )}
          </CardStat>
        )}
        <CardStat label="Withdrawable" hint="Withdrawable USDC">
          {fmtUsdOrDash(hl.balance.withdrawable_usd)}
        </CardStat>
        <CardStat label="Queue" hint="Orders the gateway has queued for this device to sign">
          {hl.queue_pending}
        </CardStat>
        <CardStat label="Network" hint="Hyperliquid network this executor is bound to">
          <span className="mono" style={{ fontSize: 12 }}>
            {hl.network}
          </span>
        </CardStat>
        <CardStat label="Last poll" hint="Time since the daemon last polled the gateway">
          <span className="mono" style={{ fontSize: 12 }}>
            {hl.last_poll_at ? timeAgo(hl.last_poll_at) : "—"}
          </span>
        </CardStat>
      </div>

      {/* SEPARATED accounts only: perp+spot are distinct wallets, so surface
          a one-click transfer to fund perp from spot without leaving for
          hyperliquid.xyz. A UNIFIED account has no transfer (HL greys it out;
          spot backs perp automatically) — never render the button there. */}
      {!isUnified &&
        (hasIdleSpot || (perpValue != null && perpValue > 0 && spotUsdc != null)) && (
        <div
          style={{
            marginTop: 10,
            display: "flex",
            alignItems: "center",
            gap: 8,
            flexWrap: "wrap",
          }}
        >
          <button
            type="button"
            className="btn sm"
            onClick={() => setTransferOpen(true)}
            disabled={busy}
          >
            <ArrowRightLeft size={13} style={{ marginRight: 4 }} />
            Transfer spot ↔ perp
          </button>
          {perpEmptySpotFunded && (
            <span style={{ fontSize: 11, color: "var(--amber)" }}>
              Your {fmtUsdOrDash(spotUsdc)} is in spot — move it to perp to trade.
            </span>
          )}
        </div>
      )}

      {transferOpen && !isUnified && (
        <SpotPerpTransferDialog
          spotUsdc={spotUsdc}
          perpUsd={perpValue}
          paper={hl.paper_mode}
          initialToPerp={perpValue === 0}
          onClose={() => setTransferOpen(false)}
          onDone={() => setTransferOpen(false)}
        />
      )}

      {hl.auth_alert === "session_expired" ? (
        <div className="error-box" style={{ marginTop: 12 }}>
          <div style={{ fontWeight: 600, marginBottom: 4 }}>Session expired</div>
          <div style={{ marginBottom: 8, opacity: 0.85 }}>
            This device's signing session expired. Re-run setup to reconnect —
            your account, balance, and open positions are unaffected.
          </div>
          <button className="btn sm" onClick={onRerunSetup}>
            Re-run setup
          </button>
        </div>
      ) : hl.auth_alert === "subscription_inactive" ? (
        <div className="error-box" style={{ marginTop: 12 }}>
          <div style={{ fontWeight: 600, marginBottom: 4 }}>
            Subscription inactive
          </div>
          <div style={{ opacity: 0.85 }}>
            Signing is paused because your subscription lapsed. Renew it to
            resume — the signer reconnects automatically, no re-setup needed.
          </div>
        </div>
      ) : hl.error ? (
        <div className="error-box" style={{ marginTop: 12 }}>{hl.error}</div>
      ) : null}

      {/* pairing block — min-height reserves space so a paired/not-paired
          flip on poll doesn't jump the card. */}
      <div style={{ marginTop: 14, minHeight: 96 }}>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 8,
            flexWrap: "wrap",
            marginBottom: 6,
          }}
        >
          <span className="hud-label">Pairing</span>
          <StatusPill tone={hl.paired ? "ok" : "muted"} dot={hl.paired}>
            {hl.paired ? "paired" : "not paired"}
          </StatusPill>
          {serverPairing && (
            <StatusPill
              tone={serverPairing.tone}
              title={
                pairing?.last_heartbeat_at
                  ? `server truth · last heartbeat ${timeAgo(pairing.last_heartbeat_at)}`
                  : "server truth"
              }
            >
              {serverPairing.label}
            </StatusPill>
          )}
          {!serverPairing && pairingErr && (
            <StatusPill tone="muted" title={pairingErr}>
              server status unavailable
            </StatusPill>
          )}
        </div>

        {hl.paired ? (
          <>
            {pairing && !pairingHealthy(pairing.state) && (
              <div className="banner warn" role="alert">
                <span style={{ flex: 1 }}>
                  The server reports this executor as{" "}
                  <strong>{PAIRING_PILL[pairing.state]?.label ?? pairing.state}</strong>
                  {pairing.state === "wallet_mismatch" && pairing.linked_address ? (
                    <>
                      . Your web account is linked to{" "}
                      <span className="mono">{shortAddr(pairing.linked_address, 6, 4)}</span>.
                      Re-run setup and pair with that wallet.
                    </>
                  ) : (
                    <>. Trades won't be delivered until pairing is fixed. Re-run setup.</>
                  )}
                </span>
              </div>
            )}
            <div className="row">
              <span className="label">Server</span>
              <span className="value mono">{hl.server_url}</span>
            </div>
            <div className="row">
              <span className="label">User</span>
              <span className="value">{hl.discord_handle ?? hl.user_id ?? "—"}</span>
            </div>
          </>
        ) : (
          <>
            <p style={{ margin: "0 0 8px" }}>
              Pair this device with DegenBox so the gateway can queue perp orders for it
              to sign. One click with a linked Discord account, or paste a connect token.
            </p>
            <div className="btn-row">
              <button className="btn primary" disabled={busy} onClick={onPair}>
                <Link2 size={14} /> Pair signer
              </button>
            </div>
          </>
        )}
      </div>

      {/* agent-key block */}
      <div style={{ marginTop: 14 }}>
        <div
          style={{
            display: "flex",
            alignItems: "center",
            gap: 8,
            flexWrap: "wrap",
            marginBottom: 6,
          }}
        >
          <span className="hud-label">Agent key</span>
          <StatusPill tone={status.hl_unlocked ? "ok" : "warn"} icon={KeyRound}>
            {status.hl_unlocked ? "unlocked" : "locked"}
          </StatusPill>
        </div>
        <div className="row">
          <span className="label">Agent address</span>
          <span
            className="value mono"
            style={{ display: "inline-flex", alignItems: "center", gap: 4 }}
          >
            {status.hl_address ? shortAddr(status.hl_address, 8, 6) : "—"}
            {status.hl_address && (
              <CopyButton text={status.hl_address} label="Copy agent address" />
            )}
          </span>
        </div>
        <p style={{ margin: "8px 0 0", fontSize: 12, color: "var(--fg-dim)" }}>
          The sandboxed API key that signs trades for your master account. It can
          never withdraw funds. Rotate it via “Re-run setup” (import a freshly minted
          key); removing it stops signing on this device, while pairing survives a
          re-import of the same key.
        </p>
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
    <div style={{ display: "grid", gap: 2, minWidth: 80 }}>
      <span className="hud-label" title={hint}>
        {label}
      </span>
      <span style={{ fontSize: 13, color: "var(--fg)", fontVariantNumeric: "tabular-nums" }}>
        {children}
      </span>
    </div>
  );
}

function Dash({ hint }: { hint: string }) {
  return (
    <span style={{ color: "var(--fg-faint)" }} title={hint}>
      —
    </span>
  );
}
