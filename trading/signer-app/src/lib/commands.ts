// Single command layer for every MONEY / MUTATING action in the app.
//
// Why this module exists (slice-2 architecture): this same UI will later
// render in three shells — the Tauri desktop (full authority, local key
// material), a remote `signer.degenbox.app` mode (same UI, commands
// RELAYED to the paired desktop, no keys in the browser), and a mobile
// responsive pass. Every action that moves money or mutates trading
// state MUST flow through here, so the remote shell can swap this one
// module's transport (IPC → relay) without touching a single surface.
//
// Today every verb delegates straight to the local Tauri `ipc`. No
// capability-flag plumbing yet — just the clean seam. READ paths
// (status polls, position lists, gateway GETs) deliberately stay on
// `ipc` / `gateway.ts`; only writes are centralised here.
//
// Grouping mirrors the future capability split: `vault` / `keys` (never
// relayed — key material is desktop-only), `perps`, `sol`, `bots`,
// `pairing`. Keep money verbs OUT of feature files' direct `ipc` use as
// call sites migrate onto `commands.*`.

import { ipc } from "../ipc";
import type {
  BotArmReq,
  ClientBudgetReq,
  ClientPresetUpdateReq,
  CreateBotSessionReq,
  HlCopyConfigFull,
  HlCopyConfigPatch,
  LegSpec,
  SolCopyConfigCreate,
  SolCopyConfigPatch,
} from "../ipc";
import { solPositionsEx } from "../features/positions/ipc";

/** One position's flatten outcome — surfaced by the emergency-flatten UI. */
export interface FlattenResult {
  label: string;
  ok: boolean;
  detail: string;
}

export const commands = {
  // ── device-wide kill-switch ───────────────────────────────────────
  /** Pause / resume ALL signing on this device (both venues). */
  setPaused: (paused: boolean) => ipc.setPaused(paused),

  // ── Perpetuals money actions ──────────────────────────────────────
  perps: {
    setPaperMode: (paper: boolean) => ipc.hlSetPaperMode(paper),
    closePosition: (coin: string, percent: number) =>
      ipc.hlClosePosition(coin, percent),
    placeTpsl: (
      coin: string,
      tpPrice: string | null,
      slPrice: string | null,
      closePercent?: number,
    ) => ipc.hlPlaceTpsl(coin, tpPrice, slPrice, closePercent),
    copyConfigCreate: (body: HlCopyConfigPatch & { target_wallet: string }) =>
      ipc.hlCopyConfigCreate(body),
    copyConfigUpdate: (configId: string, patch: HlCopyConfigPatch) =>
      ipc.hlCopyConfigUpdate(configId, patch),
    /** Reduce-only close 100% of every open perp position on this
     *  account, sequentially. The emergency money-kill. */
    async flatten(): Promise<FlattenResult[]> {
      const status = await ipc.hlStatus();
      const positions = status.balance.positions ?? [];
      const out: FlattenResult[] = [];
      for (const p of positions) {
        try {
          const r = await ipc.hlClosePosition(p.coin, 100);
          out.push({ label: p.coin, ok: true, detail: r.status ?? "submitted" });
        } catch (e) {
          out.push({ label: p.coin, ok: false, detail: String(e) });
        }
      }
      return out;
    },
  },

  // ── Solana money actions ──────────────────────────────────────────
  sol: {
    positionSell: (mint: string, fractionBps: number, ownerPubkey?: string | null) =>
      ipc.solPositionSell(mint, fractionBps, ownerPubkey),
    targetArm: (mint: string, entryPriceUsd: string, legs: LegSpec[]) =>
      ipc.solTargetArm(mint, entryPriceUsd, legs),
    targetDisarm: (mint: string) => ipc.solTargetDisarm(mint),
    copyConfigCreate: (body: SolCopyConfigCreate) => ipc.solCopyConfigCreate(body),
    copyConfigUpdate: (configId: string, patch: SolCopyConfigPatch) =>
      ipc.solCopyConfigUpdate(configId, patch),
    copyConfigDelete: (configId: string) => ipc.solCopyConfigDelete(configId),
    trackedWalletSetCopyMode: (walletId: string, copyMode: boolean) =>
      ipc.trackedWalletSetCopyMode(walletId, copyMode),
    /** Sell 100% of every open Solana spot position through this
     *  device's signer. `owner=null` → the backend resolves the holding
     *  wallet on-chain and refuses ambiguous routing (never guesses). */
    async flatten(): Promise<FlattenResult[]> {
      const positions = await solPositionsEx();
      const out: FlattenResult[] = [];
      for (const p of positions) {
        try {
          const r = await ipc.solPositionSell(p.mint, 10_000, null);
          out.push({ label: p.symbol, ok: true, detail: `tx ${r.signature.slice(0, 8)}…` });
        } catch (e) {
          out.push({ label: p.symbol, ok: false, detail: String(e) });
        }
      }
      return out;
    },
  },

  // ── Bots / fleet ──────────────────────────────────────────────────
  bots: {
    sessionCreate: (body: CreateBotSessionReq) => ipc.botSessionCreate(body),
    sessionCancel: (sessionId: string) => ipc.botSessionCancel(sessionId),
    arm: (req: BotArmReq) => ipc.botArm(req),
    disarm: (sessionId?: string) => ipc.botDisarm(sessionId),
    clientBudgetSet: (gatewayId: string, req: ClientBudgetReq) =>
      ipc.clientBudgetSet(gatewayId, req),
    clientPresetAssign: (gatewayId: string, presetId: string, enabled: boolean) =>
      ipc.clientPresetAssign(gatewayId, presetId, enabled),
    clientPresetUnassign: (gatewayId: string, presetId: string) =>
      ipc.clientPresetUnassign(gatewayId, presetId),
    clientPresetUpdate: (gatewayId: string, presetId: string, body: ClientPresetUpdateReq) =>
      ipc.clientPresetUpdate(gatewayId, presetId, body),
    clientPause: (id: string, paused: boolean) => ipc.clientPause(id, paused),
    clientSetPrimary: (id: string) => ipc.clientSetPrimary(id),
    clientLabel: (id: string, label: string | null) => ipc.clientLabel(id, label),
    clientRemove: (id: string) => ipc.clientRemove(id),
  },

  // ── Pairing (perps agent ↔ gateway) ───────────────────────────────
  pairing: {
    pair: (
      serverUrl: string,
      token: string,
      accountAddress: string,
      totpCode?: string,
      agentAddress?: string,
    ) => ipc.hlPair(serverUrl, token, accountAddress, totpCode, agentAddress),
    unpair: () => ipc.hlUnpair(),
    submitTotp: (code: string) => ipc.submitHlTotp(code),
  },

  // ── Vault + key material (NEVER relayed — desktop-only) ────────────
  vault: {
    unlock: (password: string, backend: "file" | "keychain") =>
      ipc.unlock(password, backend),
    lock: () => ipc.lock(),
  },
} as const;

export type FollowConfig = HlCopyConfigFull;
