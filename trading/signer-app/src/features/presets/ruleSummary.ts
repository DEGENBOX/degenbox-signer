// Read-only filter summary — compact human rendering of a preset's
// `rules` array, grouped by the scanner's 7 rule categories. This is
// NOT an editor: the app shows what the preset filters on; edits
// happen in the web preset studio.
//
// Coverage: every `Rule` variant in
// `crates/modules/alpha-scanner/src/filter/domain.rs` has a formatter.
// Unknown kinds (future variants) fall back to a prettified kind +
// compact params so a newer gateway never renders blanks.

import type { PresetRule } from "./ipc";

export interface RuleGroup {
  section: string;
  items: string[];
}

// ─── tiny formatters ────────────────────────────────────────────────

function num(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && v.trim() !== "") {
    const n = Number(v);
    if (Number.isFinite(n)) return n;
  }
  return null;
}

function str(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}

function arrLen(v: unknown): number {
  return Array.isArray(v) ? v.length : 0;
}

/** "$50k", "$1.2M" — compact USD. */
function usd(v: unknown): string {
  const n = num(v);
  if (n == null) return "?";
  const abs = Math.abs(n);
  if (abs >= 1_000_000_000) return `$${trim(n / 1_000_000_000)}B`;
  if (abs >= 1_000_000) return `$${trim(n / 1_000_000)}M`;
  if (abs >= 1_000) return `$${trim(n / 1_000)}k`;
  if (abs >= 1) return `$${trim(n)}`;
  return `$${n}`;
}

function pct(v: unknown): string {
  const n = num(v);
  return n == null ? "?%" : `${trim(n)}%`;
}

function trim(n: number): string {
  return String(Number(n.toPrecision(4)));
}

/** Window seconds → "5m" / "1h" / "24h" / "all-time". */
function win(v: unknown): string {
  const s = num(v);
  if (s == null) return "?";
  if (s === 0) return "all-time";
  if (s < 3600) return `${trim(s / 60)}m`;
  if (s < 86_400) return `${trim(s / 3600)}h`;
  return `${trim(s / 86_400)}d`;
}

/** Duration seconds → "30s" / "5m" / "1h2m" style (token age bounds). */
function dur(v: unknown): string {
  const s = num(v);
  if (s == null) return "?";
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${trim(s / 60)}m`;
  if (s < 86_400) return `${trim(s / 3600)}h`;
  return `${trim(s / 86_400)}d`;
}

/** Timeframe ref ("alltime" | "h24" | "d7" | "d30") → label. */
function tf(v: unknown): string {
  switch (str(v)) {
    case "h24":
      return "24h";
    case "d7":
      return "7d";
    case "d30":
      return "30d";
    case "alltime":
      return "all-time";
    default:
      return "?";
  }
}

/** "m5" | "h1" | … DexScreener window keys pass through verbatim. */
function dexWin(v: unknown): string {
  return str(v) ?? "?";
}

/** min/max range → "10–50", "≥10", "≤50", "" (both unset). */
function range(min: unknown, max: unknown, fmt: (v: unknown) => string): string {
  const lo = min == null ? null : fmt(min);
  const hi = max == null ? null : fmt(max);
  if (lo != null && hi != null) return `${lo}–${hi}`;
  if (lo != null) return `≥ ${lo}`;
  if (hi != null) return `≤ ${hi}`;
  return "any";
}

function mode(v: unknown): string {
  return str(v) === "blacklist" ? "exclude" : "only";
}

// ─── rule table ─────────────────────────────────────────────────────

const MENTIONS = "Mentions";
const CALLER = "Caller";
const TOKEN = "Token";
const SOCIALS = "Socials";
const FLOW = "Money flow";
const HOLDERS = "Holders";
const SECURITY = "Security";
const DEV = "Developer";

type Fmt = (r: PresetRule) => string;

const RULES: Record<string, { section: string; fmt: Fmt }> = {
  // mentions
  mention_first: { section: MENTIONS, fmt: () => "first mention only" },
  mentions_in_window: {
    section: MENTIONS,
    fmt: (r) =>
      `${range(r.min_unique_callers, r.max_unique_callers, (v) => String(num(v) ?? "?"))} ${
        str(r.mode) === "channels" ? "channels" : "callers"
      } / ${win(r.window_secs)}`,
  },
  channel_mentions_in_window: {
    section: MENTIONS,
    fmt: (r) => `≥ ${num(r.min_unique_channels) ?? "?"} channels / ${win(r.window_secs)}`,
  },
  mention_speed_min: {
    section: MENTIONS,
    fmt: (r) =>
      `≥ ${num(r.min_callers_per_min) ?? "?"} callers/min over ${win(r.window_secs)}`,
  },
  mention_source: {
    section: MENTIONS,
    fmt: (r) => `${mode(r.mode)} ${arrLen(r.items)} ${str(r.target) ?? "source"}(s)`,
  },

  // caller perf
  caller_calls_min: {
    section: CALLER,
    fmt: (r) => `calls ${range(r.min_calls, r.max_calls, (v) => String(num(v) ?? "?"))}`,
  },
  caller_calls_by_timeframe_min: {
    section: CALLER,
    fmt: (r) => `≥ ${num(r.min_calls) ?? "?"} calls (${tf(r.timeframe)})`,
  },
  caller_avg_gain_min: {
    section: CALLER,
    fmt: (r) => `avg gain ≥ ${pct(r.min_pct)} (${tf(r.timeframe)})`,
  },
  caller_median_gain_min: {
    section: CALLER,
    fmt: (r) => `median gain ≥ ${pct(r.min_pct)} (${tf(r.timeframe)})`,
  },
  caller_percentile_gain_min: {
    section: CALLER,
    fmt: (r) => `${str(r.tier) ?? "p?"} gain ≥ ${pct(r.min_pct)} (${tf(r.timeframe)})`,
  },
  caller_median_max_drawdown_min: {
    section: CALLER,
    fmt: (r) => `median drawdown ≥ ${pct(r.min_pct)} (${tf(r.timeframe)})`,
  },
  caller_peak_multiplier_min: {
    section: CALLER,
    fmt: (r) => `peak ≥ ${num(r.min_x) ?? "?"}×`,
  },
  caller_median_peak_multiplier_min: {
    section: CALLER,
    fmt: (r) => `median peak ≥ ${num(r.min) ?? "?"}×`,
  },
  caller_multiplier_hit_rate_min: {
    section: CALLER,
    fmt: (r) => {
      const hr = num(r.min_hit_rate);
      return `hit rate ≥ ${hr == null ? "?" : trim(hr * 100)}% @ ${num(r.at_x) ?? "?"}× (${tf(r.timeframe)})`;
    },
  },
  exclude_caller_handles: {
    section: CALLER,
    fmt: (r) => `exclude ${arrLen(r.handles)} handle(s)`,
  },
  caller_is_tracked_wallet: {
    section: CALLER,
    fmt: (r) => {
      const n = arrLen(r.addresses);
      return n === 0 ? "tracked wallets (attached)" : `tracked wallets (${n})`;
    },
  },
  wallet_source_filter: {
    section: CALLER,
    fmt: (r) => {
      const n = arrLen(r.addresses);
      return `${mode(r.mode)} ${n === 0 ? "attached" : n} wallet(s)`;
    },
  },

  // token / general
  token_age_range: {
    section: TOKEN,
    fmt: (r) => `age ${range(r.min_secs, r.max_secs, dur)}`,
  },
  mcap_min: { section: TOKEN, fmt: (r) => `mcap ≥ ${usd(r.min_usd)}` },
  mcap_max: { section: TOKEN, fmt: (r) => `mcap ≤ ${usd(r.max_usd)}` },
  fdv_range: { section: TOKEN, fmt: (r) => `FDV ${range(r.min_usd, r.max_usd, usd)}` },
  price_usd_range: {
    section: TOKEN,
    fmt: (r) => `price ${range(r.min_usd, r.max_usd, (v) => `$${num(v) ?? "?"}`)}`,
  },
  price_change_range: {
    section: TOKEN,
    fmt: (r) => `Δprice(${dexWin(r.window)}) ${range(r.min_pct, r.max_pct, pct)}`,
  },
  drawdown_from_ath_range: {
    section: TOKEN,
    fmt: (r) => `ATH drawdown ${range(r.min_pct, r.max_pct, pct)}`,
  },
  chain: { section: TOKEN, fmt: (r) => `chains: ${arrLen(r.allowed)}` },
  chain_blacklist: { section: TOKEN, fmt: (r) => `chains blocked: ${arrLen(r.blocked)}` },
  on_bonding_curve: {
    section: TOKEN,
    fmt: (r) => (r.required ? "on bonding curve" : "not on bonding curve"),
  },
  gen_bonded: {
    section: TOKEN,
    fmt: (r) => (r.required ? "graduated" : "still bonding"),
  },
  bonding_progress_range: {
    section: TOKEN,
    fmt: (r) => `bonding ${range(r.min_pct, r.max_pct, pct)}`,
  },
  gen_name: {
    section: TOKEN,
    fmt: (r) => `name ${mode(r.mode)} ${arrLen(r.patterns)} pattern(s)`,
  },
  gen_symbol: {
    section: TOKEN,
    fmt: (r) => `symbol ${mode(r.mode)} ${arrLen(r.symbols)}`,
  },
  source_dex_filter: {
    section: TOKEN,
    fmt: (r) => `DEX ${mode(r.mode)} ${arrLen(r.dex_ids)}`,
  },

  // socials
  requires_twitter: { section: SOCIALS, fmt: () => "needs Twitter" },
  requires_telegram: { section: SOCIALS, fmt: () => "needs Telegram" },
  requires_website: { section: SOCIALS, fmt: () => "needs website" },
  requires_livestream: {
    section: SOCIALS,
    fmt: (r) => (r.required ? "needs livestream" : "no livestream"),
  },
  dex_listed: {
    section: SOCIALS,
    fmt: (r) => (r.required ? "DexScreener listed" : "not DS-listed"),
  },
  dex_paid: {
    section: SOCIALS,
    fmt: (r) => (r.required ? "DS profile paid" : "DS profile unpaid"),
  },
  dex_boosted: {
    section: SOCIALS,
    fmt: (r) => (r.required ? "DS boosted" : "not DS-boosted"),
  },
  dex_boosted_amount_min: {
    section: SOCIALS,
    fmt: (r) => `boost spend ≥ ${usd(r.min)}`,
  },
  twitter_reuses_max: {
    section: SOCIALS,
    fmt: (r) => `handle reuses ≤ ${num(r.max) ?? "?"}`,
  },
  twitter_renames_max: {
    section: SOCIALS,
    fmt: (r) => `handle renames ≤ ${num(r.max) ?? "?"}`,
  },

  // money flow + volume quality
  liquidity_min: { section: FLOW, fmt: (r) => `liq ≥ ${usd(r.min_usd)}` },
  liquidity_max: { section: FLOW, fmt: (r) => `liq ≤ ${usd(r.max_usd)}` },
  volume24h_min: { section: FLOW, fmt: (r) => `vol 24h ≥ ${usd(r.min_usd)}` },
  volume24h_max: { section: FLOW, fmt: (r) => `vol 24h ≤ ${usd(r.max_usd)}` },
  volume_min: {
    section: FLOW,
    fmt: (r) => `vol(${dexWin(r.window)}) ≥ ${usd(r.min_usd)}`,
  },
  volume_max: {
    section: FLOW,
    fmt: (r) => `vol(${dexWin(r.window)}) ≤ ${usd(r.max_usd)}`,
  },
  tx_count_min: {
    section: FLOW,
    fmt: (r) => `txs(${dexWin(r.window)}) ≥ ${num(r.min) ?? "?"}`,
  },
  tx_count_max: {
    section: FLOW,
    fmt: (r) => `txs(${dexWin(r.window)}) ≤ ${num(r.max) ?? "?"}`,
  },
  buys_min: {
    section: FLOW,
    fmt: (r) => `buys(${dexWin(r.window)}) ≥ ${num(r.min) ?? "?"}`,
  },
  sells_min: {
    section: FLOW,
    fmt: (r) => `sells(${dexWin(r.window)}) ≥ ${num(r.min) ?? "?"}`,
  },
  sells_max: {
    section: FLOW,
    fmt: (r) => `sells(${dexWin(r.window)}) ≤ ${num(r.max) ?? "?"}`,
  },
  unique_buyers_min: {
    section: FLOW,
    fmt: (r) => `buyers(${dexWin(r.window)}) ≥ ${num(r.min) ?? "?"}`,
  },
  unique_traders_min: {
    section: FLOW,
    fmt: (r) => `traders(${dexWin(r.window)}) ≥ ${num(r.min) ?? "?"}`,
  },
  buy_sell_ratio_max: {
    section: FLOW,
    fmt: (r) => `buy/sell(${dexWin(r.window)}) ≤ ${num(r.max_ratio) ?? "?"}`,
  },
  flow_reserve_range: {
    section: FLOW,
    fmt: (r) => `reserve ${range(r.min_usd, r.max_usd, usd)}`,
  },
  trending_score_24h_min: {
    section: FLOW,
    fmt: (r) => `trending 24h ≥ ${num(r.min) ?? "?"}`,
  },

  // holders
  holders_count_range: {
    section: HOLDERS,
    fmt: (r) => `holders ${range(r.min, r.max, (v) => String(num(v) ?? "?"))}`,
  },
  top10_max_pct: { section: HOLDERS, fmt: (r) => `top10 ≤ ${pct(r.max_pct)}` },
  top20_max_pct: { section: HOLDERS, fmt: (r) => `top20 ≤ ${pct(r.max_pct)}` },
  top50_max_pct: { section: HOLDERS, fmt: (r) => `top50 ≤ ${pct(r.max_pct)}` },
  top100_max_pct: { section: HOLDERS, fmt: (r) => `top100 ≤ ${pct(r.max_pct)}` },
  top200_max_pct: { section: HOLDERS, fmt: (r) => `top200 ≤ ${pct(r.max_pct)}` },
  dev_max_pct: { section: HOLDERS, fmt: (r) => `dev ≤ ${pct(r.max_pct)}` },
  sniper_max_pct: { section: HOLDERS, fmt: (r) => `snipers ≤ ${pct(r.max_pct)}` },
  bundler_max_pct: { section: HOLDERS, fmt: (r) => `bundlers ≤ ${pct(r.max_pct)}` },
  fresh_max_pct: { section: HOLDERS, fmt: (r) => `fresh ≤ ${pct(r.max_pct)}` },
  smart_max_pct: { section: HOLDERS, fmt: (r) => `smart ≤ ${pct(r.max_pct)}` },
  insider_max_pct: { section: HOLDERS, fmt: (r) => `insiders ≤ ${pct(r.max_pct)}` },
  pro_max_pct: { section: HOLDERS, fmt: (r) => `pros ≤ ${pct(r.max_pct)}` },
  insider_count_max: {
    section: HOLDERS,
    fmt: (r) => `insiders ≤ ${num(r.max) ?? "?"}`,
  },
  bundler_count_max: {
    section: HOLDERS,
    fmt: (r) => `bundlers ≤ ${num(r.max) ?? "?"}`,
  },
  sniper_count_max: { section: HOLDERS, fmt: (r) => `snipers ≤ ${num(r.max) ?? "?"}` },
  fresh_count_max: { section: HOLDERS, fmt: (r) => `fresh ≤ ${num(r.max) ?? "?"}` },
  pro_count_max: { section: HOLDERS, fmt: (r) => `pros ≤ ${num(r.max) ?? "?"}` },
  smart_count_max: { section: HOLDERS, fmt: (r) => `smart ≤ ${num(r.max) ?? "?"}` },

  // security
  security_honeypot_block: { section: SECURITY, fmt: () => "no honeypots" },
  security_buy_tax_max: {
    section: SECURITY,
    fmt: (r) => `buy tax ≤ ${pct(r.max_pct)}`,
  },
  security_sell_tax_max: {
    section: SECURITY,
    fmt: (r) => `sell tax ≤ ${pct(r.max_pct)}`,
  },

  // developer
  dev_deployed_count_max: {
    section: DEV,
    fmt: (r) => `deploys ≤ ${num(r.max_count) ?? "?"}`,
  },
  dev_migration_count_max: {
    section: DEV,
    fmt: (r) => `migrations ≤ ${num(r.max_count) ?? "?"}`,
  },
  dev_address_allowlist: {
    section: DEV,
    fmt: (r) => `only ${arrLen(r.addresses)} dev(s)`,
  },
  dev_address_blocklist: {
    section: DEV,
    fmt: (r) => `exclude ${arrLen(r.addresses)} dev(s)`,
  },
};

/** Last-resort: "some_new_rule {a: 1}" → "some new rule · a 1". */
function fallback(r: PresetRule): string {
  const params = Object.entries(r)
    .filter(([k]) => k !== "kind")
    .map(([k, v]) => {
      const val = Array.isArray(v)
        ? `[${v.length}]`
        : typeof v === "object" && v !== null
          ? "{…}"
          : String(v);
      return `${k.replace(/_/g, " ")} ${val}`;
    })
    .slice(0, 3)
    .join(", ");
  const name = (r.kind || "rule").replace(/_/g, " ");
  return params ? `${name}: ${params}` : name;
}

const SECTION_ORDER = [MENTIONS, CALLER, TOKEN, SOCIALS, FLOW, HOLDERS, SECURITY, DEV];

/** Group + render a preset's rules. Sections come back in canonical
 *  order; rules keep their in-section authoring order. */
export function summarizeRules(rules: PresetRule[] | null | undefined): RuleGroup[] {
  const bySection = new Map<string, string[]>();
  for (const r of rules ?? []) {
    const meta = RULES[r.kind];
    const section = meta?.section ?? "Other";
    let label: string;
    try {
      label = meta ? meta.fmt(r) : fallback(r);
    } catch {
      label = fallback(r);
    }
    const list = bySection.get(section);
    if (list) list.push(label);
    else bySection.set(section, [label]);
  }
  const out: RuleGroup[] = [];
  for (const section of SECTION_ORDER) {
    const items = bySection.get(section);
    if (items) out.push({ section, items });
  }
  const other = bySection.get("Other");
  if (other) out.push({ section: "Other", items: other });
  return out;
}
