// features/presets — typed IPC + gateway facade for the Solana Presets
// tab (W3.2). CONSUMES src/ipc.ts and src/lib/gateway.ts only; no new
// Rust commands are referenced here. Endpoints without a bespoke Tauri
// command (full preset detail, bot_config PATCH, copy-trade summary,
// follow/unfollow, tracked-wallet create) ride the generic
// `gateway_fetch` proxy via gwGet/gwPost/gwPatch/gwDelete.
//
// NOTE on numbers: the gateway serialises rust Decimals as JSON
// *strings* (`mirrored_sol_lamports_*`, `min_source_buy_usd`, …).
// Counts (i64) arrive as plain JSON numbers.

import { fmtSol } from "@degenbox/ui";
import { summarizeStoredLadder } from "../../components/LadderSpecEditor";
import { gwDelete, gwGet, gwPatch, gwPost } from "../../lib/gateway";
import {
  ipc,
  type ClientInfo,
  type ClientPreset,
  type ClientPresetUpdateReq,
  type CopytradeConfig,
  type LegSpec,
  type SolCopyConfigCreate,
  type SolCopyConfigFull,
  type SolCopyConfigPatch,
  type TrackedWallet,
} from "../../ipc";

export { ipc };
export type {
  ClientInfo,
  ClientPreset,
  ClientPresetUpdateReq,
  CopytradeConfig,
  LegSpec,
  SolCopyConfigCreate,
  SolCopyConfigFull,
  SolCopyConfigPatch,
  TrackedWallet,
};

export const LAMPORTS = 1e9;

/** Where "Edit filters on website" lands — the web preset studio. */
export const WEB_PRESETS_URL = "https://staging.degenbox.app/alpha/presets";

// ─── Scanner presets (alpha-scanner module) ────────────────────────

/** One rule from `alpha_presets.rules.rules` — internally tagged
 * (`{"kind":"mentions_in_window", …}`). Open-ended on purpose: the
 * read-only summary renders known kinds nicely and falls back to a
 * prettified kind for anything the backend adds later. */
export interface PresetRule {
  kind: string;
  [key: string]: unknown;
}

/** Full preset row from `GET /api/alpha/presets` (gateway `PresetView`
 * — the flattened `Preset` + hoisted `notifications_enabled`). Only the
 * fields this surface reads are typed. */
export interface AlphaPresetFull {
  id: string;
  owner_user_id: string;
  name: string;
  color: string | null;
  rules: { rules: PresetRule[] };
  /** Opaque JSONB — execution keys parsed via `parseBotConfig`. */
  bot_config: unknown;
  is_active: boolean;
  version: number;
  dedupe_enabled: boolean;
  dedupe_duration_minutes: number | null;
  include_monitors: boolean;
  is_public: boolean;
  notifications_enabled: boolean;
  attached_wallet_addresses: string[];
  created_at: string;
  updated_at: string;
}

export const fetchAlphaPresets = () =>
  gwGet<AlphaPresetFull[]>("/api/alpha/presets");

/** PATCH only the bot_config blob (execution + sell strategy). The
 * endpoint REPLACES the whole JSONB when set — callers must merge the
 * non-trading keys first (see `mergeBotConfig`). */
export const patchPresetBotConfig = (
  presetId: string,
  botConfig: Record<string, unknown> | null,
) =>
  gwPatch<AlphaPresetFull>(`/api/alpha/presets/${presetId}`, {
    bot_config: botConfig,
  });

// ─── Copy-trade summary + follow toggle (W2.3 endpoints) ───────────

export interface CopyTradeSummary {
  configs_total: number;
  configs_enabled: number;
  configs_disabled: number;
  intents_total: number;
  intents_published: number;
  intents_rejected: number;
  /** Decimal-as-string lamports (sum(bigint) is numeric in PG). */
  mirrored_sol_lamports_24h: string | null;
  mirrored_sol_lamports_7d: string | null;
  mirrored_sol_lamports_all: string | null;
  last_intent_at: string | null;
}

export interface CopyTradeWalletStat {
  wallet_address: string;
  intents_count: number;
  intents_published: number;
  intents_rejected: number;
  mirrored_sol_lamports: string | null;
  last_intent_at: string | null;
  enabled: boolean | null;
}

export interface CopyTradeSummaryView {
  summary: CopyTradeSummary;
  per_wallet: CopyTradeWalletStat[];
}

export const fetchCopySummary = () =>
  gwGet<CopyTradeSummaryView>("/api/trading/copy-trade/summary");

/** Enable a config as a live follow (idempotent, owner-scoped). */
export const followCopyConfig = (configId: string) =>
  gwPost<SolCopyConfigFull>(`/api/trading/copy-trade/configs/${configId}/follow`);

/** Disable without deleting — re-follow later keeps every setting. */
export const unfollowCopyConfig = (configId: string) =>
  gwDelete<SolCopyConfigFull>(`/api/trading/copy-trade/configs/${configId}/follow`);

// ─── Tracked-wallet create (paste-to-follow flow) ──────────────────

/** Track a pasted leader address on the fly so a copy config can bind
 * it (`POST /api/wallet-tracker/wallets`). `copy_mode: true` arms the
 * sandwich-guard bypass the copy engine needs. 409 = already tracked —
 * callers resolve via `ipc.trackedWalletsList()` instead. */
export const createTrackedWallet = (address: string, alias?: string) =>
  gwPost<{ id: string; address: string; alias: string | null; copy_mode: boolean }>(
    "/api/wallet-tracker/wallets",
    { address, alias: alias ?? null, copy_mode: true },
  );

/** Base58 shape check mirroring the gateway's validator (32–44 chars,
 * Bitcoin alphabet — no 0/O/I/l). */
export function isSolanaAddress(s: string): boolean {
  return /^[1-9A-HJ-NP-Za-km-z]{32,44}$/.test(s.trim());
}

// ─── Display helpers ───────────────────────────────────────────────

/** Lamports number → SOL input text ("0.1", "2.5") for form seeding. */
export function solText(lamports: number | null | undefined): string {
  if (lamports == null) return "";
  return String(Number((lamports / LAMPORTS).toPrecision(12)));
}

/** One-line human summary of a copy config's buy sizing — covers ALL
 * four `sizing_mode`s (the old table cell showed modes 2/3 as a bogus
 * "0 SOL"). Shared by the copy-trade table and the Running-now list. */
export function sizingSummary(c: SolCopyConfigFull): string {
  switch (c.sizing_mode) {
    case 1:
      return `${((c.pct_of_balance_bps ?? 0) / 100).toFixed(1)}% of my balance`;
    case 2: {
      const pct = c.buy_size_pct ?? 100;
      return pct === 100 ? "same size as the leader" : `${pct}% of the leader's buy`;
    }
    case 3: {
      const pct = c.balance_pct ?? 100;
      return pct === 100
        ? "matches the leader's conviction"
        : `${pct}% of the leader's conviction`;
    }
    default:
      return `${fmtSol(c.fixed_sol_lamports ?? 0)} SOL per copy`;
  }
}

/** Short human summary of how a copy config exits positions. */
export function sellSummary(c: SolCopyConfigFull): string {
  const ladder = summarizeStoredLadder(c.default_ladder);
  if (c.mirror_sells && ladder) return "sells with the leader + own ladder";
  if (c.mirror_sells) return "sells with the leader";
  if (ladder) return `own ladder ${ladder}`;
  return "manual sells";
}
