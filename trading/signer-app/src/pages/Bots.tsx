// Bots (Solana) — the client/bot management surface (W3.3 redesign).
// One hairline card per client wallet: identity, runtime, balance,
// budget caps and the client's own auto-buy sessions (sessions belong
// to clients via their wallet_pubkey). Creating a client attaches a
// wallet — generated in-app, imported key, or an adopted extension
// keystore (the attach matrix lives in features/bots/ipc.ts).
//
// Session contract preserved from the old page: Start creates the
// gateway budget row (POST /api/trading/bot/sessions) and ARMS this
// device's engine (/bot/enable); Stop disarms the local engine FIRST,
// then cancels the server row. 10 s polling, optimistic mutations —
// the fleet poll is owned by SolBotsTab (shared with Running now).

import { useState } from "react";
import { Plus, Users } from "lucide-react";
import { save as saveFileDialog } from "@tauri-apps/plugin-dialog";
import { shortAddr } from "@degenbox/ui";
import { DangerConfirm, EmptyHero } from "../components/ui";
import {
  ipc,
  type BotPreset,
  type ClientInfo,
  type StatusReport,
} from "../features/bots/ipc";
import { groupSessions } from "../features/bots/meta";
import type { Fleet } from "../features/bots/useFleet";
import { ClientCard } from "../features/bots/ClientCard";
import { SessionList } from "../features/bots/SessionList";
import { CreateClientDialog } from "../features/bots/CreateClientDialog";
import { ActivateDialog } from "../features/bots/ActivateDialog";
import { BudgetDialog } from "../features/bots/BudgetDialog";
import { StartSessionDialog } from "../features/bots/StartSessionDialog";

export function Bots({
  status,
  embedded,
  fleet,
}: {
  status: StatusReport | null;
  embedded?: boolean;
  /** Owned by the parent tab (SolBotsTab) so Running now and this
   * surface read the same 10 s snapshot. */
  fleet: Fleet;
}) {
  const { clients, balances, sessions, device, err, busyId } = fleet;

  const [notice, setNotice] = useState<{ kind: "ok" | "warn"; text: string } | null>(null);
  const [sessionBusyId, setSessionBusyId] = useState<string | null>(null);

  // Dialog state.
  const [createOpen, setCreateOpen] = useState(false);
  const [budgetFor, setBudgetFor] = useState<ClientInfo | null>(null);
  const [sessionFor, setSessionFor] = useState<ClientInfo | null>(null);
  const [activateFor, setActivateFor] = useState<ClientInfo | null>(null);
  const [removeFor, setRemoveFor] = useState<ClientInfo | null>(null);
  const [removeErr, setRemoveErr] = useState<string | null>(null);
  const [removeBusy, setRemoveBusy] = useState(false);

  const unlocked = status?.sol_unlocked ?? false;
  const primaryAddress = status?.sol_pubkey ?? null;
  const armedIds = new Set(device?.armed_session_ids ?? []);
  const grouped = groupSessions(sessions, clients ?? []);

  // ── session actions (contract preserved from the old page) ──────
  const arm = async (s: BotPreset) => {
    if (!s.preset_id) {
      setNotice({
        kind: "warn",
        text: "This session has no preset. The engine needs a preset's signal stream; start a fresh session with a preset selected.",
      });
      return;
    }
    setSessionBusyId(s.id);
    setNotice(null);
    try {
      await ipc.botArm({
        session_id: s.id,
        preset_id: s.preset_id,
        per_trade_lamports: s.per_trade_lamports,
        budget_lamports: s.budget_lamports,
        spent_lamports: s.spent_lamports,
        per_token_cap_lamports: s.per_token_cap_lamports,
        tip_lamports: s.tip_lamports,
      });
      setNotice({
        kind: "ok",
        text: `Session armed on this device. Auto-buys fire on “${s.name}” signals until budget or expiry.`,
      });
    } catch (e) {
      setNotice({ kind: "warn", text: `Arming failed: ${e}` });
    } finally {
      setSessionBusyId(null);
      await fleet.reload();
    }
  };

  const stop = async (s: BotPreset) => {
    setSessionBusyId(s.id);
    setNotice(null);
    try {
      // Disarm the local engine FIRST so trading stops immediately;
      // "not armed here" is fine — the server cancel still runs.
      await ipc.botDisarm(s.id).catch(() => {});
      await ipc.botSessionCancel(s.id);
      setNotice({ kind: "ok", text: `Session “${s.name}” stopped.` });
    } catch (e) {
      setNotice({ kind: "warn", text: `Stop failed: ${e}` });
    } finally {
      setSessionBusyId(null);
      await fleet.reload();
    }
  };

  // ── client actions ───────────────────────────────────────────────
  const exportKeystore = async (c: ClientInfo) => {
    try {
      const dest = await saveFileDialog({
        title: "Save encrypted keystore backup",
        defaultPath: `degenbox-sol-${c.address.slice(0, 6)}.json`,
        filters: [{ name: "Keystore", extensions: ["json"] }],
      });
      if (typeof dest === "string" && dest) {
        await ipc.clientExportKeystore(c.id, dest);
        setNotice({ kind: "ok", text: "Encrypted keystore exported." });
      }
    } catch (e) {
      setNotice({ kind: "warn", text: `Export failed: ${e}` });
    }
  };

  const remove = async () => {
    if (!removeFor) return;
    setRemoveBusy(true);
    setRemoveErr(null);
    try {
      await ipc.clientRemove(removeFor.id);
      setRemoveFor(null);
      await fleet.reload();
    } catch (e) {
      setRemoveErr(String(e));
    } finally {
      setRemoveBusy(false);
    }
  };

  return (
    <>
      <div className="page-head">
        <div>
          {!embedded && (
            <>
              <h1>Bots</h1>
              <p className="page-sub">
                Your Solana clients: each one a wallet sealed in this device's vault, with
                its own budget caps and auto-buy sessions. Execution runs in this app's
                signer engine.
              </p>
            </>
          )}
        </div>
        <span className="page-actions">
          <button className="btn primary" onClick={() => setCreateOpen(true)}>
            <Plus size={13} /> Client
          </button>
        </span>
      </div>

      {/* The old KPI strip + "nothing armed" banner moved up into the
          tab's Running-now section (R4) — one place answers "is it
          live?", this section is for managing wallets + sessions. */}

      {notice && (
        <div className={`banner ${notice.kind === "warn" ? "warn" : ""}`} role="status">
          <span style={{ flex: 1 }}>{notice.text}</span>
          <button className="btn" onClick={() => setNotice(null)}>
            Dismiss
          </button>
        </div>
      )}

      {err && <div className="error-box">{err}</div>}

      {clients !== null && clients.length === 0 ? (
        <EmptyHero
          icon={<Users size={22} />}
          title="No clients yet"
          desc={
            <>
              Add the first wallet this device should trade with: generate a fresh one,
              paste a private key, or re-attach a signer wallet. Keys are encrypted into
              one vault under your master passphrase and never leave this machine.
            </>
          }
          action={
            <button className="btn primary lg" onClick={() => setCreateOpen(true)}>
              <Plus size={15} /> New client
            </button>
          }
        />
      ) : (
        (clients ?? []).map((c, i) => (
          <ClientCard
            key={c.id}
            c={c}
            index={i}
            balance={balances[c.address] ?? null}
            sessions={grouped.byWallet.get(c.address) ?? []}
            sessionsLoading={sessions === null}
            armedIds={armedIds}
            unlocked={unlocked}
            busy={busyId === c.id}
            sessionBusy={sessionBusyId !== null}
            onToggleActive={fleet.toggleActive}
            onRename={fleet.rename}
            onSetPrimary={fleet.setPrimary}
            onActivate={(x) => setActivateFor(x)}
            onExport={exportKeystore}
            onRemove={(x) => {
              setRemoveErr(null);
              setRemoveFor(x);
            }}
            onBudget={(x) => setBudgetFor(x)}
            onStartSession={(x) => setSessionFor(x)}
            onArm={arm}
            onStop={stop}
          />
        ))
      )}

      {clients === null && !err && (
        <section className="card" aria-busy>
          <span className="skeleton" style={{ width: "40%", height: 16 }} />
          <div style={{ marginTop: 10 }}>
            <span className="skeleton" style={{ width: "70%" }} />
          </div>
        </section>
      )}

      {grouped.unbound.length > 0 && (
        <section className="card">
          <div className="card-title">
            Unattached sessions
            <span className="right">
              <span
                className="hud-label brackets"
                title="Sessions whose wallet matches no client on this device (another device's wallet, or one that was removed)"
              >
                {grouped.unbound.length}
              </span>
            </span>
          </div>
          <SessionList
            sessions={grouped.unbound}
            armedIds={armedIds}
            unlocked={unlocked}
            busy={sessionBusyId !== null}
            walletIsPrimary={false}
            onArm={arm}
            onStop={stop}
          />
        </section>
      )}

      <CreateClientDialog
        open={createOpen}
        onClose={() => setCreateOpen(false)}
        onDone={() => fleet.reload()}
      />
      <BudgetDialog
        client={budgetFor}
        onClose={() => setBudgetFor(null)}
        onSaved={() => fleet.reload()}
      />
      <StartSessionDialog
        client={sessionFor}
        primaryAddress={primaryAddress}
        onClose={() => setSessionFor(null)}
        onStarted={async (sessionName, armErr) => {
          setSessionFor(null);
          setNotice(
            armErr
              ? {
                  kind: "warn",
                  text: `Session created on the server but NOT armed on this device: ${armErr}. Fix the issue, then click arm on the row.`,
                }
              : {
                  kind: "ok",
                  text: `Session started + armed on this device. Auto-buys fire on “${sessionName}” signals.`,
                },
          );
          await fleet.reload();
        }}
      />
      <ActivateDialog
        client={activateFor}
        onClose={() => setActivateFor(null)}
        onDone={() => fleet.reload()}
      />
      <DangerConfirm
        open={removeFor !== null}
        title="Remove client"
        phrase="REMOVE"
        busy={removeBusy}
        error={removeErr}
        onCancel={() => setRemoveFor(null)}
        onConfirm={remove}
      >
        <p style={{ marginTop: 0 }}>
          Removes{" "}
          <strong>
            {removeFor?.label ?? (removeFor ? shortAddr(removeFor.address, 6, 6) : "")}
          </strong>{" "}
          <span className="mono">
            ({removeFor ? shortAddr(removeFor.address, 5, 5) : ""})
          </span>{" "}
          from this device and stops its runtime. The encrypted keystore is kept on
          disk as <span className="mono">.removed.bak</span>, but without an exported
          backup, losing this machine loses the wallet.
        </p>
      </DangerConfirm>
    </>
  );
}
