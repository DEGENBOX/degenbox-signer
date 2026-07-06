// Perpetuals one-strategy-per-wallet slot model (operator PFLICHT rule,
// Perps only). Each executor wallet holds EXACTLY ONE strategy —
// copytrade OR caller-follow, never both. This card shows the wallet's
// single strategy slot; "Replace" clears it (so a new one can be picked
// in the sections below); running a SECOND strategy needs a SECOND API
// wallet via the guided flow.
//
// Conflict surfacing: if the gateway currently has BOTH a copy config
// and caller subs enabled (a state the old UI allowed), the slot flags
// the violation and offers one-click resolution.
//
// BACKEND GAP (reported): a second HL executor wallet isn't wired —
// Hyperliquid runs one master account + one agent key today. The
// "New API wallet" flow is designed here but its provisioning step is a
// clear stub (TODO: multi-agent provisioning endpoint). Everything else
// (slot detection, replace/clear) is live.

import { useCallback, useEffect, useState } from "react";
import { KeyRound, Plus, RefreshCw, Repeat, Users, Wallet } from "lucide-react";
import { StatusPill } from "@degenbox/ui";
import { DangerConfirm, Modal, shortAddr } from "../../components/ui";
import { commands } from "../../lib/commands";
import { ipc, type HlStatus, type StatusReport } from "../../ipc";
import { fetchSubs, patchSub, type ExecSubscription } from "../perps-presets/ipc";
import type { HlCopyConfigFull } from "../../ipc";

type Slot =
  | { kind: "empty" }
  | { kind: "copytrade"; config: HlCopyConfigFull }
  | { kind: "callers"; subs: ExecSubscription[] }
  | { kind: "conflict"; config: HlCopyConfigFull; subs: ExecSubscription[] };

interface Props {
  status: StatusReport | null;
  hl: HlStatus | null;
}

export function WalletStrategySlot({ status, hl }: Props) {
  const [slot, setSlot] = useState<Slot | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [clearOpen, setClearOpen] = useState(false);
  const [newWalletOpen, setNewWalletOpen] = useState(false);

  const load = useCallback(async () => {
    try {
      const [configs, subs] = await Promise.all([ipc.hlCopyConfigsFull(), fetchSubs()]);
      const enabledCopy = configs.find((c) => c.enabled) ?? null;
      const enabledSubs = subs.filter((s) => s.venue === "hyperliquid" && s.enabled);
      if (enabledCopy && enabledSubs.length > 0) {
        setSlot({ kind: "conflict", config: enabledCopy, subs: enabledSubs });
      } else if (enabledCopy) {
        setSlot({ kind: "copytrade", config: enabledCopy });
      } else if (enabledSubs.length > 0) {
        setSlot({ kind: "callers", subs: enabledSubs });
      } else {
        setSlot({ kind: "empty" });
      }
      setErr(null);
    } catch (e) {
      setErr(String(e));
    }
  }, []);

  useEffect(() => {
    load();
    const id = setInterval(load, 15000);
    return () => clearInterval(id);
  }, [load]);

  // Clear the slot = disable the current strategy so the wallet is free
  // to take a new one (picked in the Callers / Copy-trade sections).
  const clearSlot = async () => {
    if (!slot || slot.kind === "empty") return;
    setBusy(true);
    setErr(null);
    try {
      if (slot.kind === "copytrade" || slot.kind === "conflict") {
        await commands.perps.copyConfigUpdate(slot.config.id, { enabled: false });
      }
      if (slot.kind === "callers" || slot.kind === "conflict") {
        for (const s of slot.subs) {
          await patchSub(s.id, { enabled: false });
        }
      }
      setClearOpen(false);
      await load();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const wallet = status?.hl_address ?? hl?.agent_address ?? null;
  const account = hl?.account_address ?? null;

  return (
    <section className="card corners slot-card" aria-label="Strategy slot">
      <div className="slot-head">
        <span className="slot-wallet">
          <Wallet size={15} />
          <span>
            <span className="slot-wallet-name">Executor wallet</span>
            <span className="slot-wallet-addr mono">
              {wallet ? shortAddr(wallet, 6, 4) : "no agent key"}
              {account && (
                <span className="slot-wallet-master" title="Master account it trades for">
                  {" · "}
                  {shortAddr(account, 4, 4)}
                </span>
              )}
            </span>
          </span>
        </span>
        <button className="btn sm" onClick={() => setNewWalletOpen(true)}>
          <Plus size={12} /> New API wallet
        </button>
      </div>

      {err && <div className="error-box" style={{ marginTop: 10 }}>{err}</div>}

      <div className="slot-body">
        <span className="hud-label">Strategy slot</span>
        <SlotContent slot={slot} onClear={() => setClearOpen(true)} busy={busy} />
      </div>

      <p className="slot-rule">
        One wallet runs exactly one strategy. To run a second strategy in parallel,
        create a second API wallet.
      </p>

      <DangerConfirm
        open={clearOpen}
        title="Clear strategy slot"
        phrase="clear"
        busy={busy}
        error={err}
        onCancel={() => setClearOpen(false)}
        onConfirm={clearSlot}
      >
        <p style={{ marginTop: 0 }}>
          This disables the wallet's current strategy so the slot is free. The config
          itself is kept (disabled). You can re-enable it or attach a different
          strategy below.
        </p>
      </DangerConfirm>

      <NewApiWalletDialog open={newWalletOpen} onClose={() => setNewWalletOpen(false)} />
    </section>
  );
}

function SlotContent({
  slot,
  onClear,
  busy,
}: {
  slot: Slot | null;
  onClear: () => void;
  busy: boolean;
}) {
  if (slot === null) {
    return <span className="slot-loading">…</span>;
  }
  if (slot.kind === "empty") {
    return (
      <div className="slot-filled empty">
        <span className="slot-empty-label">Empty (no strategy attached)</span>
        <span className="slot-empty-hint">
          Attach a copy-trade leader or follow callers in the sections below.
        </span>
      </div>
    );
  }
  if (slot.kind === "conflict") {
    return (
      <div className="slot-filled conflict">
        <div className="slot-strategy">
          <StatusPill tone="danger">rule violated</StatusPill>
          <span className="slot-strategy-name">
            Both copytrade AND {slot.subs.length} caller
            {slot.subs.length === 1 ? "" : "s"} are active
          </span>
        </div>
        <button className="btn sm danger" onClick={onClear} disabled={busy}>
          <Repeat size={12} /> Resolve (clear slot)
        </button>
      </div>
    );
  }
  return (
    <div className="slot-filled">
      <div className="slot-strategy">
        {slot.kind === "copytrade" ? (
          <>
            <StatusPill tone="ok" icon={RefreshCw}>
              copytrade
            </StatusPill>
            <span className="slot-strategy-name mono">
              {shortAddr(slot.config.target_wallet, 6, 4)} · {Number(slot.config.scale_factor).toFixed(2)}×
            </span>
          </>
        ) : (
          <>
            <StatusPill tone="ok" icon={Users}>
              callers
            </StatusPill>
            <span className="slot-strategy-name">
              {slot.subs.length} caller{slot.subs.length === 1 ? "" : "s"} followed
            </span>
          </>
        )}
      </div>
      <button className="btn sm" onClick={onClear} disabled={busy} title="Free this slot to attach a different strategy">
        <Repeat size={12} /> Replace
      </button>
    </div>
  );
}

// Guided create-a-second-API-wallet flow. UI is complete; the actual
// provisioning is a documented stub — HL runs one agent key today.
function NewApiWalletDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  return (
    <Modal open={open} onClose={onClose} title="New API wallet" width={460}>
      <ol className="wallet-steps">
        <li>
          <span className="wallet-step-n">1</span>
          <div>
            <strong>Mint an agent key</strong>
            <p>
              On app.hyperliquid.xyz/API, create a fresh API agent key for your master
              account. Agent keys can trade but never withdraw.
            </p>
          </div>
        </li>
        <li>
          <span className="wallet-step-n">2</span>
          <div>
            <strong>Import &amp; name it</strong>
            <p>Paste the agent private key; give the wallet a label for its strategy.</p>
          </div>
        </li>
        <li>
          <span className="wallet-step-n">3</span>
          <div>
            <strong>Attach one strategy</strong>
            <p>Assign either a copy-trade leader or a caller-follow set, one per wallet.</p>
          </div>
        </li>
      </ol>
      <div className="banner warn" role="note" style={{ marginTop: 4 }}>
        <KeyRound size={15} style={{ flexShrink: 0 }} />
        <span style={{ flex: 1 }}>
          {/* TODO(slice-3): wire multi-agent provisioning. Hyperliquid runs one
              master account + one agent key today, so a second parallel executor
              wallet needs a gateway endpoint (register N agent keys per user) that
              does not exist yet. This flow is the designed UX; the provisioning
              step is stubbed. */}
          <strong>Not yet wired.</strong> Multiple executor wallets need a gateway
          change (one master account supports one agent key today). This is the
          planned flow. Provisioning lands in a follow-up.
        </span>
      </div>
      <div className="modal-foot">
        <button className="btn" onClick={onClose}>
          Close
        </button>
      </div>
    </Modal>
  );
}
