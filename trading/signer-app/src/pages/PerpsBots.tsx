// Bots tab, Perpetuals mode (W4 redesign) — replaces the W1 shim that
// embedded the old HlOverview. The perps side has no multi-bot fleet:
// THIS DEVICE is the sole executor, so the page presents it as one
// first-class bot card in the Sol Bots layout (KPI strip → numbered
// sections → ClientCard-style ExecutorCard → terminal signing feed).
//
// Operational contract preserved 1:1 from HlOverview — zero-state
// wizard funnel, server-pairing truth poll (15 s), paper mode,
// re-run setup, unpair + remove-agent-key danger confirms — restated
// in the W3 card idiom. UI label is "Perpetuals"; code ids stay hl.

import { useEffect, useState } from "react";
import { KeyRound } from "lucide-react";
import { DangerConfirm, EmptyHero, Kpi, fmtUsd, timeAgo } from "../components/ui";
import { HlWizard } from "../components/HlWizard";
import { ShellSection } from "../components/ShellSection";
import {
  ipc,
  type HlPairingStatus,
  type HlStatus,
  type StatusReport,
} from "../features/perps-bots/ipc";
import { connMeta, pairingHealthy } from "../features/perps-bots/meta";
import { ExecutorCard } from "../features/perps-bots/ExecutorCard";
import { SignFeed } from "../features/perps-bots/SignFeed";

interface Props {
  status: StatusReport | null;
  hl: HlStatus | null;
  onReload: () => void;
  /** Rendered inside the BOTS tab wrapper — suppress the page title. */
  embedded?: boolean;
}

export function PerpsBots({ status, hl, onReload, embedded }: Props) {
  const [wizardOpen, setWizardOpen] = useState(false);
  const [unpairOpen, setUnpairOpen] = useState(false);
  const [removeOpen, setRemoveOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [removeErr, setRemoveErr] = useState<string | null>(null);

  const hasKey = !!status?.hl_address;

  // Server-side pairing truth — the gateway can disagree with the local
  // "paired" flag (wallet mismatch, server-side revoke). Polled gently;
  // null = no token yet or the gateway predates the endpoint.
  const [pairing, setPairing] = useState<HlPairingStatus | null>(null);
  const [pairingErr, setPairingErr] = useState<string | null>(null);
  useEffect(() => {
    let alive = true;
    const probe = async () => {
      try {
        const p = await ipc.hlPairingStatus();
        if (alive) {
          setPairing(p);
          setPairingErr(null);
        }
      } catch (e) {
        if (alive) setPairingErr(String(e));
      }
    };
    probe();
    const id = setInterval(probe, 15000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  const run = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setErr(null);
    try {
      await fn();
      onReload();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const unpair = async () => {
    setBusy(true);
    setErr(null);
    try {
      await ipc.hlUnpair();
      setUnpairOpen(false);
      onReload();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  const removeKey = async () => {
    setBusy(true);
    setRemoveErr(null);
    try {
      await ipc.removeHlKeystore();
      setRemoveOpen(false);
      onReload();
    } catch (e) {
      setRemoveErr(String(e));
    } finally {
      setBusy(false);
    }
  };

  // ── zero state: no agent key in the vault yet ────────────────────
  if (status && !hasKey) {
    return (
      <>
        {!embedded && (
          <>
            <h1>Bots</h1>
            <p className="page-sub">
              This device is your Perpetuals executor. Every perp order DegenBox queues
              is signed here.
            </p>
          </>
        )}
        <EmptyHero
          icon={<KeyRound size={22} />}
          title="No Perpetuals agent key yet"
          desc={
            <>
              Mint an API agent key at app.hyperliquid.xyz/API, paste its private key
              here and pair this device with DegenBox. The agent can trade for your
              master account but never withdraw.
            </>
          }
          action={
            <button className="btn primary lg" onClick={() => setWizardOpen(true)}>
              <KeyRound size={15} /> Set up Perpetuals
            </button>
          }
        />
        <HlWizard
          open={wizardOpen}
          onClose={() => setWizardOpen(false)}
          onDone={onReload}
          status={status}
          hl={hl}
        />
      </>
    );
  }

  // ── derived KPIs ─────────────────────────────────────────────────
  const conn = connMeta(hl);
  const gatewayTone: "pos" | "neg" | undefined =
    !hl || hl.conn === "connecting" || hl.conn === "paused"
      ? undefined
      : hl.conn === "ready"
        ? "pos"
        : "neg";
  const serverDegraded = hl?.paired && pairing != null && !pairingHealthy(pairing.state);

  return (
    <>
      {!embedded && (
        <>
          <h1>Bots</h1>
          <p className="page-sub">
            This device is your Perpetuals executor: pairing, agent key, gateway health
            and the signing feed for every perp order DegenBox queues.
          </p>
        </>
      )}

      <div className="kpi-strip">
        <Kpi
          label="Gateway"
          value={
            <span style={{ display: "inline-flex", alignItems: "center", gap: 8 }}>
              <span className={`status-dot ${conn.dot} ${conn.pulse ? "pulse" : ""}`} />
              {conn.label}
            </span>
          }
          tone={gatewayTone}
          loading={hl === null}
          sub={hl?.last_poll_at ? `last poll ${timeAgo(hl.last_poll_at)}` : undefined}
        />
        <Kpi
          label="Queue"
          value={hl ? hl.queue_pending : "—"}
          loading={hl === null}
          sub="pending orders"
        />
        <Kpi
          label="Account value"
          value={fmtUsd(hl?.balance.account_value_usd)}
          loading={hl === null}
          sub={
            hl?.balance.fetched_at ? `updated ${timeAgo(hl.balance.fetched_at)}` : undefined
          }
        />
        <Kpi
          label="Withdrawable"
          value={fmtUsd(hl?.balance.withdrawable_usd)}
          loading={hl === null}
        />
      </div>

      {serverDegraded && (
        <div className="banner warn" role="alert">
          <span style={{ flex: 1 }}>
            The gateway reports this executor's pairing as degraded. Trades won't be
            delivered until it's fixed. Details on the executor card below.
          </span>
        </div>
      )}

      {err && <div className="error-box">{err}</div>}

      <ShellSection num="01" title="Executor">
        {status && hl ? (
          <ExecutorCard
            status={status}
            hl={hl}
            pairing={pairing}
            pairingErr={pairingErr}
            busy={busy}
            onToggleSigning={(next) => run(() => ipc.setPaused(!next))}
            onPaperMode={(next) => run(() => ipc.hlSetPaperMode(next))}
            onRerunSetup={() => setWizardOpen(true)}
            onPair={() => setWizardOpen(true)}
            onUnpair={() => setUnpairOpen(true)}
            onRemoveKey={() => {
              setRemoveErr(null);
              setRemoveOpen(true);
            }}
          />
        ) : (
          <section className="card corners" aria-busy>
            <span className="skeleton" style={{ width: "40%", height: 16 }} />
            <div style={{ marginTop: 10 }}>
              <span className="skeleton" style={{ width: "70%" }} />
            </div>
          </section>
        )}
      </ShellSection>

      <ShellSection num="02" title="Signing feed">
        <SignFeed />
      </ShellSection>

      <HlWizard
        open={wizardOpen}
        onClose={() => setWizardOpen(false)}
        onDone={onReload}
        status={status}
        hl={hl}
      />
      <DangerConfirm
        open={unpairOpen}
        title="Unpair signer"
        phrase="unpair"
        busy={busy}
        error={err}
        onCancel={() => setUnpairOpen(false)}
        onConfirm={unpair}
      >
        <p>
          This removes the pairing token and master account from this device and stops
          the signing daemon. Your encrypted agent key stays, so you can pair again any
          time.
        </p>
      </DangerConfirm>
      <DangerConfirm
        open={removeOpen}
        title="Remove agent key"
        phrase="remove"
        busy={busy}
        error={removeErr}
        onCancel={() => setRemoveOpen(false)}
        onConfirm={removeKey}
      >
        <p>
          This deletes the encrypted agent keystore from this machine and stops the
          daemon. Agent keys are sandboxed. You can mint a fresh one on
          app.hyperliquid.xyz at any time, so this is safe but disruptive.
        </p>
      </DangerConfirm>
    </>
  );
}
