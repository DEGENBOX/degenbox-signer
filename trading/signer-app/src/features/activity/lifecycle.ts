// Shared lifecycle classification for the signer-app's live Bot-activity
// feed — the desktop twin of the web Bot tabs
// (frontend/modules/hyperliquid/src/exchange/botActivity.ts +
//  frontend/modules/trading/src/botActivity.ts).
//
// The gateway serves the SAME instruction-lifecycle rows to the desktop
// (via gateway_fetch → `/api/hyperliquid/exchange/bot/activity` and
// `/api/trading/bot/activity`), so the app now shows what its bot is
// doing — and why — with the exact status language the web uses:
// emerald filled · amber pending · rose failed · ink muted expired.
//
// These row shapes mirror the Rust `BotActivityRow`s; Decimals arrive as
// numeric strings — coerce with Number() at render.

export type Tone = "good" | "warn" | "bad" | "info" | "muted";

// ─── Perpetuals rows (hyperliquid/exchange/api.rs BotActivityRow) ──

export type SignerStatus = "pending" | "delivered" | "acked" | "expired";
export type OrderLifecycle =
  | "queued"
  | "submitted"
  | "partial"
  | "filled"
  | "cancelled"
  | "failed";
export type HlBotKind =
  | "entry"
  | "sl"
  | "tp"
  | "close"
  | "cancel"
  | "leverage"
  | "other";

export interface HlActivityRow {
  cloid: string;
  kind: HlBotKind;
  coin: string | null;
  side: "buy" | "sell" | null;
  size_usd: string | null;
  reduce_only: boolean | null;
  leverage: number | null;
  oid: number | null;
  signer_status: SignerStatus;
  order_status: OrderLifecycle | null;
  filled_size_usd: string | null;
  closed_pnl: string | null;
  err_msg: string | null;
  signal_id: string | null;
  caller_id: string | null;
  caller_name: string | null;
  target_wallet: string | null;
  created_at: string;
  delivered_at: string | null;
  acked_at: string | null;
  filled_at: string | null;
  expired_at: string | null;
}

// ─── Solana rows (trading/api/bot_activity.rs BotActivityRow) ──────

export type SolActivityKind = "intent" | "skip";
export type SolActivitySource = "signal" | "copytrade" | "manual";

export interface SolActivityRow {
  id: string;
  kind: SolActivityKind | string;
  side: "buy" | "sell" | null;
  mint: string | null;
  symbol: string | null;
  name: string | null;
  image_url: string | null;
  amount_in_lamports: number | null;
  status: string;
  source: SolActivitySource | string;
  source_label: string | null;
  target_wallet: string | null;
  reason: string | null;
  signature: string | null;
  submit_mode: string | null;
  created_at: string;
}

export interface LifecycleStage {
  label: string;
  tone: Tone;
  /** Terminal (no more movement expected) — suppresses the live pulse. */
  terminal: boolean;
  /** Furthest of 3 stages lit (queued → working → done). */
  reached: number;
  /** Longer tooltip explaining the raw signer + order status. */
  detail: string;
}

/** Collapse the (signer_status, order_status) pair into one honest
 *  effective stage + tone. Ported 1:1 from the web HL feed. */
export function hlLifecycle(row: HlActivityRow): LifecycleStage {
  const o = row.order_status;
  const s = row.signer_status;
  const raw = `signer: ${s}${o ? ` · order: ${o}` : " · no order row"}`;

  if (o === "failed")
    return { label: "Failed", tone: "bad", terminal: true, reached: 1, detail: raw };
  if (o === "cancelled")
    return { label: "Cancelled", tone: "muted", terminal: true, reached: 1, detail: raw };
  if (s === "expired")
    return {
      label: "Expired",
      tone: "muted",
      terminal: true,
      reached: 1,
      detail: `${raw} (the signer never picked this up in time)`,
    };
  if (o === "filled")
    return { label: "Filled", tone: "good", terminal: true, reached: 3, detail: raw };
  if (o === "partial")
    return { label: "Partial", tone: "warn", terminal: false, reached: 2, detail: raw };
  if (o === "submitted")
    return { label: "Working", tone: "info", terminal: false, reached: 2, detail: raw };
  if (s === "acked")
    return { label: "Done", tone: "good", terminal: true, reached: 3, detail: raw };
  if (s === "delivered")
    return {
      label: "Claimed",
      tone: "info",
      terminal: false,
      reached: 2,
      detail: `${raw} (the signer has it and is submitting)`,
    };
  if (s === "pending")
    return {
      label: "Queued",
      tone: "warn",
      terminal: false,
      reached: 1,
      detail: `${raw} (waiting for your signer to pick it up)`,
    };
  return { label: s, tone: "muted", terminal: false, reached: 1, detail: raw };
}

export function hlKindLabel(kind: HlBotKind): string {
  switch (kind) {
    case "entry":
      return "Open";
    case "sl":
      return "Stop";
    case "tp":
      return "Take-profit";
    case "close":
      return "Close";
    case "cancel":
      return "Cancel";
    case "leverage":
      return "Leverage";
    default:
      return "Order";
  }
}

/** Collapse the intent/skip status into one honest chip + tone. Ported
 *  1:1 from the web Solana feed. */
export function solLifecycle(row: SolActivityRow): LifecycleStage {
  switch (row.status) {
    case "filled":
      return { label: "Filled", tone: "good", terminal: true, reached: 3, detail: "filled" };
    case "submitted":
      return { label: "Working", tone: "info", terminal: false, reached: 2, detail: "submitted" };
    case "pending":
      return { label: "Queued", tone: "warn", terminal: false, reached: 1, detail: "queued" };
    case "failed":
      return { label: "Failed", tone: "bad", terminal: true, reached: 1, detail: "failed" };
    case "cancelled":
      return { label: "Cancelled", tone: "muted", terminal: true, reached: 1, detail: "cancelled" };
    case "expired":
      return { label: "Expired", tone: "muted", terminal: true, reached: 1, detail: "expired" };
    case "skipped":
      return { label: "Skipped", tone: "muted", terminal: true, reached: 1, detail: "skipped" };
    default:
      return { label: row.status, tone: "muted", terminal: true, reached: 1, detail: row.status };
  }
}

/** Friendly label for a Solana reject/skip reason token — mirrors the
 *  web `reasonLabel` so the desktop and web speak the same language. */
export function solReasonLabel(reason?: string | null): string {
  const r = (reason ?? "").toLowerCase();
  if (!reason) return "Skipped";
  if (r === "client_paused") return "Bot paused";
  if (r === "session_budget_exceeded") return "Session budget hit";
  if (r === "per_trade_cap_exceeded") return "Per-trade cap";
  if (r.includes("min")) return "Below min size";
  if (r.includes("allowlist")) return "Coin not allowed";
  if (r.includes("no open copied position")) return "Nothing to sell";
  if (r.includes("zero")) return "Sized to zero";
  return reason.replace(/[_:]+/g, " ").replace(/^\w/, (c) => c.toUpperCase());
}

export function solSourceLabel(row: SolActivityRow): string {
  if (row.source === "copytrade")
    return row.source_label ? `copy ${row.source_label}` : "copy-follow";
  if (row.source === "signal") return row.source_label ?? "signal";
  return "manual";
}

export function solTokenLabel(row: SolActivityRow): string {
  if (row.symbol) return row.symbol;
  if (!row.mint) return "—";
  return row.mint.length <= 10
    ? row.mint
    : `${row.mint.slice(0, 4)}…${row.mint.slice(-4)}`;
}

/** Builder-DEX markets arrive dex-qualified ("xyz:GOLD"); show the bare
 *  symbol + an optional dex tag (the web uses splitDexMarket, which is
 *  not a dep here — this covers the same shape). */
export function splitMarket(coin: string): { symbol: string; dex: string | null } {
  const i = coin.indexOf(":");
  if (i <= 0) return { symbol: coin, dex: null };
  return { symbol: coin.slice(i + 1), dex: coin.slice(0, i) };
}
