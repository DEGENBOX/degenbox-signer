// Data ownership for the Bots tab: ONE 10-second poll covering the
// vault⊕gateway client merge, per-wallet SOL balances, the backend bot
// sessions and this device's armed-engine truth. Mutations apply
// optimistically and reconcile (or revert) on the next load — the same
// inflight-guard pattern the old Home.tsx used.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  ipc,
  type BotDeviceStatus,
  type BotPreset,
  type ClientInfo,
  type SolWalletBalance,
} from "./ipc";
import { isRemote, solClients, sortClients } from "./meta";

const POLL_MS = 10_000;

export interface Fleet {
  /** Sol clients only, sorted (local→remote, primary first). */
  clients: ClientInfo[] | null;
  /** wallet address → live RPC balance. */
  balances: Record<string, SolWalletBalance>;
  sessions: BotPreset[] | null;
  device: BotDeviceStatus | null;
  /** First-load error — after the first good snapshot stale data wins. */
  err: string | null;
  busyId: string | null;
  reload: () => Promise<void>;
  setErr: (e: string | null) => void;
  // Optimistic per-client mutations.
  toggleActive: (c: ClientInfo, active: boolean) => Promise<void>;
  rename: (c: ClientInfo, label: string) => Promise<void>;
  setPrimary: (c: ClientInfo) => Promise<void>;
}

export function useFleet(): Fleet {
  const [clients, setClients] = useState<ClientInfo[] | null>(null);
  const [balances, setBalances] = useState<Record<string, SolWalletBalance>>({});
  const [sessions, setSessions] = useState<BotPreset[] | null>(null);
  const [device, setDevice] = useState<BotDeviceStatus | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);

  // Suppress poll overwrites while an optimistic mutation is in flight.
  const inflight = useRef(0);
  const haveData = useRef(false);

  const reload = useCallback(async () => {
    const [list, rows, dev] = await Promise.allSettled([
      ipc.clientsList(),
      ipc.botPresets(),
      ipc.botDeviceStatus(),
    ]);

    if (list.status === "fulfilled") {
      haveData.current = true;
      const sols = sortClients(solClients(list.value));
      if (inflight.current === 0) {
        setClients(sols);
        setErr(null);
      }
      // Per-wallet balance fan-out (local wallets have a key; remote
      // rows are address-only but the RPC read works for any pubkey).
      const addrs = sols.filter((c) => c.address).map((c) => c.address);
      const bals = await Promise.allSettled(addrs.map((a) => ipc.solBalance(a)));
      const map: Record<string, SolWalletBalance> = {};
      bals.forEach((r, i) => {
        if (r.status === "fulfilled") map[addrs[i]] = r.value;
      });
      if (Object.keys(map).length > 0) {
        setBalances((prev) => ({ ...prev, ...map }));
      }
    } else if (!haveData.current) {
      setErr(String(list.reason));
    }

    if (rows.status === "fulfilled") setSessions(rows.value);
    else if (!haveData.current) setErr((e) => e ?? String(rows.reason));
    if (dev.status === "fulfilled") setDevice(dev.value);
  }, []);

  useEffect(() => {
    reload();
    const id = setInterval(reload, POLL_MS);
    return () => clearInterval(id);
  }, [reload]);

  const mutate = useCallback(
    async (c: ClientInfo, patch: Partial<ClientInfo>, op: () => Promise<unknown>) => {
      setBusyId(c.id);
      inflight.current += 1;
      setClients((prev) =>
        prev ? prev.map((x) => (x.id === c.id ? { ...x, ...patch } : x)) : prev,
      );
      try {
        await op();
        setErr(null);
      } catch (e) {
        setErr(String(e));
      } finally {
        inflight.current -= 1;
        setBusyId(null);
        await reload(); // reconcile (or revert on error)
      }
    },
    [reload],
  );

  const toggleActive = useCallback(
    (c: ClientInfo, active: boolean) =>
      mutate(c, { paused: !active }, () => ipc.clientPause(c.id, !active)),
    [mutate],
  );

  const rename = useCallback(
    (c: ClientInfo, label: string) =>
      mutate(c, { label: label || null }, () => ipc.clientLabel(c.id, label || null)),
    [mutate],
  );

  const setPrimary = useCallback(
    async (c: ClientInfo) => {
      if (isRemote(c)) return;
      setBusyId(c.id);
      try {
        await ipc.clientSetPrimary(c.id);
        setErr(null);
      } catch (e) {
        setErr(String(e));
      } finally {
        setBusyId(null);
        await reload();
      }
    },
    [reload],
  );

  return {
    clients,
    balances,
    sessions,
    device,
    err,
    busyId,
    reload,
    setErr,
    toggleActive,
    rename,
    setPrimary,
  };
}
