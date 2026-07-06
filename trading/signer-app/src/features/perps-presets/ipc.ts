// features/perps-presets — typed gateway facade for the Perpetuals
// Presets tab (W4.2). CONSUMES src/ipc.ts and src/lib/gateway.ts only;
// no new Rust commands. Two backend surfaces:
//
//   * Callers — `/api/signals/parser/callers` (catalog) +
//     `/api/exec/subscriptions` (the user's per-caller execution
//     settings, venue-filtered to `hyperliquid`). Subscriptions are
//     upsert-on-POST (PK user/caller/venue), PATCH rides
//     `POST /api/exec/subscriptions/{id}`, unsubscribe = DELETE.
//   * Copy trade — `/api/hyperliquid/copy-trade/*` CRUD. List/create/
//     patch keep their typed Tauri commands (src/ipc.ts); summary,
//     follow/unfollow and delete ride the generic gateway proxy.
//
// NOTE on numbers: the gateway serialises rust Decimals as JSON
// *strings* (`size_multiplier`, `mirrored_usd_*`, `default_size_usd`,
// …). Counts (i64) arrive as plain JSON numbers — coerce per field
// with `Number(...)`.

import { gwDelete, gwGet, gwPost } from "../../lib/gateway";
import {
  ipc,
  type CopytradeConfig,
  type HlCopyConfigFull,
  type HlCopyConfigPatch,
} from "../../ipc";

export { ipc };
export type { CopytradeConfig, HlCopyConfigFull, HlCopyConfigPatch };

// ─── Callers (signal-parser module) ────────────────────────────────

export type CallerType = "user" | "role" | "wallet" | "telegram" | "twitter";

/** One row from `GET /api/signals/parser/callers` — the flattened
 * `CallerRow` + avatar/server enrichment. Only the fields this surface
 * reads are typed; Decimals arrive as strings. */
export interface ParserCaller {
  id: string;
  caller_id: string;
  display_name: string;
  caller_type: CallerType;
  role_name: string | null;
  wallet_address: string | null;
  telegram_username: string | null;
  twitter_handle: string | null;
  default_leverage: number | null;
  default_size_usd: string | null;
  size_low_usd: string | null;
  size_high_usd: string | null;
  enabled: boolean;
  owner_user_id: string | null;
  last_signal_at: string | null;
  last_message_at: string | null;
  created_at: string;
  updated_at: string;
  avatar_url: string | null;
  server_name: string | null;
}

export const fetchCallers = () =>
  gwGet<ParserCaller[]>("/api/signals/parser/callers");

// ─── Caller subscriptions (execution-computer module) ──────────────

export type ExecVenue = "solana_spot" | "hyperliquid" | "polymarket";

/** Ramp-in tier-table entry — `min_signal` threshold → `mult`. */
export interface TierTableEntry {
  min_signal: number;
  mult: number;
}

/** A row from `exec_user_caller_subs` (`GET /api/exec/subscriptions`).
 * Decimal columns are strings; bps columns are integers. */
export interface ExecSubscription {
  id: string;
  user_id: string;
  caller_id: string;
  venue: ExecVenue;
  enabled: boolean;
  leverage_override: number | null;
  size_usd_override: string | null;
  size_multiplier: string;
  max_leverage: number | null;
  max_size_usd: string | null;
  market_whitelist: string[] | null;
  market_blacklist: string[] | null;
  sizing_mode: number;
  sizing_pct_equity_bps: number | null;
  leverage_cap: number | null;
  market_filter_mode: number;
  market_filter_list: string[];
  skip_dca: boolean;
  drawdown_stop_pct: number | null;
  slippage_limit_bps: number;
  manual_sl_action: number;
  manual_sl_pct: number | null;
  size_basis: number;
  tier_table_json: TierTableEntry[] | null;
  zone_strategy: number;
  tp_close_percent_bps: number;
  margin_mode: number | null;
  dca_size_multiplier: string | null;
  max_position_usd: string | null;
  size_low_percent: number | null;
  size_normal_percent: number | null;
  size_high_percent: number | null;
  size_meaning: number;
  client_id: string | null;
  created_at: string;
  updated_at: string;
}

/** Body for `POST /api/exec/subscriptions` (Rust `CreateSubReq`).
 * The POST is an UPSERT on (user, caller, venue) and overwrites the
 * full override set — the editor always sends every field. */
export interface CreateSubBody {
  caller_id: string;
  venue: ExecVenue;
  enabled?: boolean;
  leverage_override?: number | null;
  size_usd_override?: string | null;
  size_multiplier?: string;
  max_leverage?: number | null;
  max_size_usd?: string | null;
  sizing_mode?: number | null;
  sizing_pct_equity_bps?: number | null;
  leverage_cap?: number | null;
  market_filter_mode?: number | null;
  market_filter_list?: string[] | null;
  skip_dca?: boolean | null;
  drawdown_stop_pct?: number | null;
  slippage_limit_bps?: number | null;
  manual_sl_action?: number | null;
  manual_sl_pct?: number | null;
  size_basis?: number | null;
  tier_table_json?: TierTableEntry[] | null;
  zone_strategy?: number | null;
  tp_close_percent_bps?: number | null;
  margin_mode?: number | null;
  dca_size_multiplier?: string | null;
  max_position_usd?: string | null;
  size_low_percent?: number | null;
  size_normal_percent?: number | null;
  size_high_percent?: number | null;
  size_meaning?: number | null;
}

export const fetchSubs = () =>
  gwGet<ExecSubscription[]>("/api/exec/subscriptions");

export const upsertSub = (body: CreateSubBody) =>
  gwPost<ExecSubscription>("/api/exec/subscriptions", body);

/** Body for `POST /api/exec/subscriptions/{id}` (Rust `PatchSubReq`).
 * PATCH semantics per field: OMITTED = keep the stored value, explicit
 * `null` = CLEAR the column, value = set. The editor uses this for
 * existing subs so blanking a field genuinely unsets it (the POST
 * upsert coalesces nulls back to the stored value and can't clear). */
export type PatchSubBody = Partial<Omit<CreateSubBody, "caller_id" | "venue">>;

export const patchSub = (id: string, body: PatchSubBody) =>
  gwPost<ExecSubscription>(`/api/exec/subscriptions/${id}`, body);

export const deleteSub = (id: string) =>
  gwDelete<void>(`/api/exec/subscriptions/${id}`);

/** Recent signed instructions (audit feed) — drives the "executions"
 * KPI. The endpoint returns the newest 200 across venues. */
export interface ExecInstructionLite {
  id: string;
  caller_id: string;
  venue: ExecVenue;
  status: "pending" | "delivered" | "acked" | "executed" | "rejected" | "expired";
  created_at: string;
  executed_at: string | null;
}

export const fetchInstructions = () =>
  gwGet<ExecInstructionLite[]>("/api/exec/instructions");

// ─── Copy-trade summary + follow toggle (HL module) ────────────────

export interface HlCopySummary {
  configs_total: number;
  configs_enabled: number;
  configs_disabled: number;
  intents_total: number;
  intents_confirmed: number;
  intents_failed: number;
  intents_rejected: number;
  /** Decimal-as-string USD sums; null when no intents in window. */
  mirrored_usd_24h: string | null;
  mirrored_usd_7d: string | null;
  mirrored_usd_all: string | null;
  last_intent_at: string | null;
}

export interface HlCopyWalletStat {
  target_wallet: string;
  intents_count: number;
  mirrored_usd: string | null;
  last_intent_at: string | null;
  enabled: boolean | null;
}

export interface HlCopySummaryView {
  summary: HlCopySummary;
  per_wallet: HlCopyWalletStat[];
}

export const fetchHlCopySummary = () =>
  gwGet<HlCopySummaryView>("/api/hyperliquid/copy-trade/summary");

/** Enable a config as the live follow — the gateway enforces the
 * single-follow invariant and 409s with a clear message. */
export const followHlConfig = (configId: string) =>
  gwPost<HlCopyConfigFull>(`/api/hyperliquid/copy-trade/configs/${configId}/follow`);

/** Disable without deleting — re-follow later keeps every setting. */
export const unfollowHlConfig = (configId: string) =>
  gwDelete<HlCopyConfigFull>(`/api/hyperliquid/copy-trade/configs/${configId}/follow`);

/** Delete the config row (no typed Tauri command exists for this). */
export const deleteHlConfig = (configId: string) =>
  gwDelete<void>(`/api/hyperliquid/copy-trade/configs/${configId}`);

// ─── Helpers ───────────────────────────────────────────────────────

/** EVM-style address shape the perps venue uses. */
export function isHlAddress(s: string): boolean {
  return /^0x[0-9a-fA-F]{40}$/.test(s.trim());
}

/** The gateway proxy surfaces HTTP errors as strings like
 * `POST /api/…: gateway 409 Conflict: {"error":"already_following",…}`.
 * Pull the embedded JSON `message` out so the user reads a sentence,
 * not transport jargon. Falls back to the raw string. */
export function friendlyGatewayError(e: unknown): string {
  const raw = e instanceof Error ? e.message : String(e);
  const brace = raw.indexOf("{");
  if (brace >= 0) {
    try {
      const parsed = JSON.parse(raw.slice(brace)) as Record<string, unknown>;
      if (typeof parsed["message"] === "string") return parsed["message"];
      if (typeof parsed["error"] === "string") return parsed["error"];
    } catch {
      // not JSON — fall through to the raw text
    }
  }
  return raw;
}
