// features/presets — typed IPC + gateway facade for the Solana Presets
// tab (W3.2). CONSUMES src/ipc.ts and src/lib/gateway.ts only; no new
// Rust commands are referenced here. Endpoints without a bespoke Tauri
// command (full preset detail, bot_config PATCH) ride the generic
// `gateway_fetch` proxy via gwGet/gwPatch.
//
// NOTE on numbers: the gateway serialises rust Decimals as JSON
// *strings* (`min_source_buy_usd`, …). Counts (i64) arrive as plain
// JSON numbers.

import { gwGet, gwPatch } from "../../lib/gateway";
import {
  ipc,
  type ClientInfo,
  type ClientPreset,
  type ClientPresetUpdateReq,
  type CopytradeConfig,
  type LegSpec,
  type TrackedWallet,
} from "../../ipc";

export { ipc };
export type {
  ClientInfo,
  ClientPreset,
  ClientPresetUpdateReq,
  CopytradeConfig,
  LegSpec,
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
