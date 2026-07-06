// Thin wrapper around `@tauri-apps/api/core` invoke so the rest of
// the app gets fully-typed RPC calls. Every Rust command in
// `src-tauri/src/commands.rs`, `src-tauri/src/sol/commands.rs` and
// `src-tauri/src/auth.rs` has a matching signature here.
//
// All surfaces are LIVE — the former mock signal feed was removed with
// the Wave-6 IA split (a polished money app shows no fake data; the
// real signal-lifecycle feed needs a gateway endpoint first, tracked
// in BACKLOG).

import { invoke } from "@tauri-apps/api/core";

export type Health = "green" | "amber" | "red";

export interface StatusReport {
  health: Health;
  paused: boolean;
  hl_unlocked: boolean;
  sol_unlocked: boolean;
  hl_address: string | null;
  sol_pubkey: string | null;
  version: string;
}

export interface OnboardingState {
  needs_onboarding: boolean;
  has_hl_keystore: boolean;
  has_sol_keystore: boolean;
  backend: "file" | "keychain" | null;
}

export interface RecentSign {
  at: string;
  /** Which runtime produced the event — drives the per-chain feeds. */
  chain: "sol" | "hl";
  kind: string;
  identifier: string;
  status: string;
}

export type HlConn = "offline" | "connecting" | "ready" | "paused" | "error";

export interface HlPosition {
  coin: string;
  szi: string;
  side: string;
  unrealized_pnl: string | null;
  entry_px: string | null;
}

export interface HlBalance {
  account_value_usd: string | null;
  withdrawable_usd: string | null;
  positions: HlPosition[];
  fetched_at: string | null;
  error: string | null;
}

export interface HlTotpPrompt {
  challenge_id: string;
  expires_at: string;
}

export interface HlStatus {
  conn: HlConn;
  paired: boolean;
  paper_mode: boolean;
  user_id: string | null;
  discord_handle: string | null;
  agent_address: string | null;
  account_address: string | null;
  server_url: string;
  network: string;
  queue_pending: number;
  last_poll_at: string | null;
  error: string | null;
  balance: HlBalance;
  totp_prompt: HlTotpPrompt | null;
}

export interface HlPairResult {
  user_id: string;
  agent_address: string;
  discord_handle: string | null;
  needs_totp: boolean;
}

/** Server-side pairing truth (`GET /signer/pairing`). */
export interface HlPairingStatus {
  state:
    | "not_registered"
    | "revoked"
    | "pending_approval"
    | "unpaired"
    | "wallet_mismatch"
    | "paired_offline"
    | "paired_live"
    | (string & {});
  linked_address: string | null;
  paired_with_account: string | null;
  agent_address: string | null;
  last_heartbeat_at: string | null;
  live: boolean;
}

// ─── Discord desktop auth (src-tauri/src/auth.rs) ─────────────────

export interface DiscordStatus {
  linked: boolean;
  /** A login was started; its browser callback hasn't arrived yet. */
  pending: boolean;
  discord_id: string | null;
  username: string | null;
  avatar: string | null;
  expires_at: string | null;
  expired: boolean;
  gateway: string | null;
  /** Last login failure, user-readable. */
  error: string | null;
}

/** One gateway access probe (`access_check`, src-tauri/src/auth.rs) —
 * drives the W1 access-loss lock. `revoked` = the gateway answered
 * 401/403 on `/auth/me` (sub expired / token revoked): the shell locks
 * the keystores. `unreachable` / `no_auth` never lock. */
export interface AccessCheck {
  state: "ok" | "no_auth" | "revoked" | "unreachable" | (string & {});
  detail: string | null;
  /** Raw `/auth/me` payload when `state === "ok"` — serialized gateway
   * AuthClaims. Read defensively: the shape is the server's. */
  me?: {
    sub?: string;
    discord_id?: string;
    discord_handle?: string;
    roles?: string[];
    /** Unix seconds */
    exp?: number;
  } | null;
}

// ─── Solana surface types (must match src-tauri/src/sol/) ────────

export interface SolPosition {
  mint: string;
  symbol: string;
  amount_ui: string;
  cost_usd: string | null;
  value_usd: string | null;
  pnl_usd: string | null;
  /** Attribution is not carried on gateway position rows yet — null. */
  source: "manual" | "bot" | "copytrade" | null;
  opened_at: string | null;
  /** Live token price (USD) — entry autofill for the TP/SL dialog. */
  current_price_usd: string | null;
}

// ─── TP/SL ladders (wire shapes mirror the gateway's targets.rs) ───

export type TriggerKind = "tp" | "sl";

/** One leg in the PUT body / default ladders — EXACT web wire shape. */
export interface LegSpec {
  kind: TriggerKind;
  trigger_pct: string;
  sell_fraction_bps: number;
}

export interface TargetLegRow {
  id: string;
  target_id: string;
  kind: TriggerKind;
  trigger_pct: string;
  sell_fraction_bps: number;
  status: "active" | "firing" | "filled" | "cancelled" | (string & {});
  triggered_at: string | null;
  triggered_price_usd: string | null;
}

/** Ladder head, flattened + legs attached (gateway `TargetLadder`). */
export interface PositionTargetRow {
  id: string;
  mint: string;
  entry_price_usd: string;
  tp_pct: string | null;
  sl_pct: string | null;
  sell_fraction_bps: number;
  status: "active" | "firing" | "triggered" | "cancelled" | (string & {});
  triggered_at: string | null;
  created_at: string;
  legs?: TargetLegRow[];
}

export function isLiveTargetStatus(status: string): boolean {
  return status === "active" || status === "firing";
}

export interface SellResult {
  signature: string;
  sold_raw: string;
}

export interface SolWalletBalance {
  sol_ui: string;
  /** No SOL price feed in the signer — always null today. */
  usd_value: string | null;
}

export interface TpLeg {
  /** percent of the position sold at this leg, 0-100 */
  pct: number;
  /** price multiple that triggers the leg, e.g. 2 = 2x entry */
  multiple: number;
}

/** A backend bot session (`trading_bot_sessions`) + its preset name. */
export interface BotPreset {
  id: string;
  name: string;
  enabled: boolean;
  chain: "solana";
  buy_sol: string;
  budget_sol: string;
  spent_sol: string;
  tp_ladder: TpLeg[];
  sl_pct: number | null;
  /** Lifetime fills for the session (the backend has no 24h split). */
  fill_count: number;
  expires_at: string | null;
  // Raw fields for the in-app arm/clone flows.
  preset_id: string | null;
  wallet_pubkey: string | null;
  per_trade_lamports: number;
  budget_lamports: number;
  spent_lamports: number;
  per_token_cap_lamports: number | null;
  tip_lamports: number | null;
}

/** Scanner preset (id + name) for the start-session form. */
export interface PresetLite {
  id: string;
  name: string;
}

/** Create body for `POST /api/trading/bot/sessions` (web wire shape). */
export interface CreateBotSessionReq {
  preset_id?: string;
  wallet_pubkey: string;
  budget_lamports: number;
  per_trade_lamports: number;
  per_token_cap_lamports?: number;
  tip_lamports?: number;
  expires_at_unix_ms: number;
  default_ladder?: LegSpec[];
}

export interface BotArmReq {
  session_id: string;
  preset_id: string;
  per_trade_lamports: number;
  budget_lamports: number;
  spent_lamports?: number;
  per_token_cap_lamports?: number | null;
  tip_lamports?: number | null;
}

/** Which sessions THIS device's engine is armed for (daemon truth). */
export interface BotDeviceStatus {
  running: boolean;
  unlocked: boolean;
  armed_session_ids: string[];
}

export interface CopytradeConfig {
  id: string;
  label: string;
  venue: "hyperliquid" | "solana";
  leader: string;
  enabled: boolean;
  size_mode: "fixed_usd" | "mirror_pct" | "equity_pct" | "fixed_sol" | "pct_balance";
  /** HL configs cap in USD … */
  max_position_usd: string | null;
  /** … Solana configs cap in SOL. At most one is set. */
  max_position_sol: string | null;
  copied_24h: number;
  last_copy_at: string | null;
}

// ─── Full copy-config editing (Sol + HL) ──────────────────────────

/** Full Sol copy config — every field the gateway stores. */
export interface SolCopyConfigFull {
  id: string;
  tracked_wallet_id: string;
  leader: string;
  label: string;
  enabled: boolean;
  /** 0 = fixed SOL · 1 = % of my balance · 2 = % of the leader's buy
   *  (`buy_size_pct`) · 3 = leader balance fraction (`balance_pct`). */
  sizing_mode: number;
  fixed_sol_lamports: number | null;
  pct_of_balance_bps: number | null;
  /** Mode 2: integer ≥ 1, 100 = mirror the leader's size. */
  buy_size_pct: number | null;
  /** Mode 3: integer ≥ 1, 100 = mirror the leader's conviction. */
  balance_pct: number | null;
  max_position_sol_lamports: number | null;
  /** Per-config copy budget in lamports; null = uncapped. */
  copy_budget_lamports: number | null;
  /** Spend counts from this instant (manual reset bumps it). */
  copy_budget_epoch: string | null;
  /** Buy each token at most once per config. */
  single_buy_per_token: boolean;
  min_source_buy_usd: string | null;
  per_mint_cooldown_secs: number;
  slippage_bps: number;
  mirror_sells: boolean;
  /** Legacy `LegSpec[]` array OR a LadderSpec v2 object. */
  default_ladder: LegSpec[] | Record<string, unknown> | null;
  client_id: string | null;
  /** Wallet's copy feed — enabled config + feed off = silently dark. */
  wallet_copy_mode: boolean;
}

export interface TrackedWallet {
  id: string;
  address: string;
  alias: string | null;
  copy_mode: boolean;
}

/** Create body for `POST /api/trading/copy/configs`. */
export interface SolCopyConfigCreate {
  tracked_wallet_id: string;
  client_id?: string;
  enabled: boolean;
  sizing_mode: number;
  fixed_sol_lamports?: number;
  pct_of_balance_bps?: number;
  buy_size_pct?: number;
  balance_pct?: number;
  max_position_sol_lamports?: number | null;
  copy_budget_lamports?: number;
  single_buy_per_token?: boolean;
  min_source_buy_usd?: string | null;
  per_mint_cooldown_secs: number;
  slippage_bps: number;
  mirror_sells: boolean;
  /** Legacy `LegSpec[]` array or a LadderSpec v2 object. */
  default_ladder?: LegSpec[] | Record<string, unknown> | null;
}

/** PATCH body — partial, with the gateway's clear-flag semantics. */
export interface SolCopyConfigPatch {
  enabled?: boolean;
  sizing_mode?: number;
  fixed_sol_lamports?: number;
  pct_of_balance_bps?: number;
  buy_size_pct?: number;
  balance_pct?: number;
  max_position_sol_lamports?: number;
  clear_max_position?: boolean;
  copy_budget_lamports?: number;
  clear_copy_budget?: boolean;
  /** Bump the budget epoch — spend counts from now on. */
  reset_copy_budget?: boolean;
  single_buy_per_token?: boolean;
  min_source_buy_usd?: string;
  clear_min_source_buy?: boolean;
  per_mint_cooldown_secs?: number;
  slippage_bps?: number;
  mirror_sells?: boolean;
  default_ladder?: LegSpec[] | Record<string, unknown>;
  clear_default_ladder?: boolean;
}

/** Full HL copy config (mirror of the web's `CopyTradeConfig`). */
export interface HlCopyConfigFull {
  id: string;
  target_wallet: string;
  scale_factor: string;
  max_position_usd: string | null;
  coin_allowlist: string[];
  min_fill_usd: string | null;
  mirror_closes: boolean;
  enabled: boolean;
  leverage_cap?: number | null;
  drawdown_stop_pct?: number | null;
  slippage_limit_bps?: number;
  follow_mode?: number;
  /** Mode-3 (fixed USD per copy) size — Decimal-as-string, null for
   * the other modes. The gateway requires it when follow_mode = 3. */
  fixed_size_usd?: string | null;
  sl_placement_strategy?: number;
  sl_placement_pct?: number | null;
  tp_placement_strategy?: number;
  tp_levels_json?: Array<{ mult: number; close_pct: number }> | null;
  retry_on_reject?: number;
  equity_basis?: number;
  created_at: string;
}

/** PATCH body for an HL copy config — any subset. */
export type HlCopyConfigPatch = Partial<
  Omit<HlCopyConfigFull, "id" | "target_wallet" | "created_at">
>;

/** PUT body for a per-client preset assignment (PATCH semantics). */
export interface ClientPresetUpdateReq {
  enabled?: boolean;
  buy_size_lamports_override?: number;
  ladder_override?: LegSpec[];
  clear_buy_size_lamports_override?: boolean;
  clear_ladder_override?: boolean;
}

export interface SolRuntimeStatus {
  /** `auth_expired` = gateway rejected our credentials (401/403);
   * execution is down until the user re-logs in (Account tab). */
  state: "offline" | "waiting_auth" | "connecting" | "ready" | "auth_expired" | "error";
  user_id: string | null;
  copy_armed: boolean;
  copy_session_sol: number | null;
  copy_spent_sol: number;
  sells_executed: number;
  copies_executed: number;
  events_failed: number;
  last_event_at: string | null;
  /** Engine liveness stamp (ready + every 30 s + every event). */
  alive_at: string | null;
  error: string | null;
}

export interface SolExecConfig {
  /** RETIRED (v0.3.0 slice 8) — copy budgets are per-config on the
   * server now. Still on the wire for older configs; gates nothing. */
  copy_session_sol: number | null;
  /** RETIRED (v0.3.0 slice 8) — see above. */
  copy_per_token_sol: number | null;
  /** Effective RPC URL (override → env → public default). */
  rpc_url: string;
  /** The explicit config override, when set (input value). */
  rpc_url_override: string | null;
  /** What the RPC resolves to WITHOUT the override (placeholder). */
  rpc_url_default: string;
  slippage_bps: number;
  tip_lamports: number;
  submit_mode: string;
}

/** Execution-parameter update — null/empty = reset to default. */
export interface SolExecParams {
  rpc_url: string | null;
  slippage_bps: number | null;
  tip_lamports: number | null;
  submit_mode: string | null;
}

export interface CliKeystoreInfo {
  path: string;
  pubkey: string;
}

// ─── Multi-wallet clients (src-tauri/src/clients.rs) ──────────────

/** Per-client budget (`BudgetView`) — USD figures are decimal-as-string. */
export interface GatewayBudget {
  session_budget_usd: string | number | null;
  max_position_usd: string | number | null;
  default_size_usd: string | number | null;
  session_budget_lamports: number | null;
  per_trade_lamports: number | null;
}

/** The HL single-active-config slot. */
export interface GatewayActiveConfig {
  type: "caller" | "copytrade" | (string & {});
  ref_id: string;
  since: string | null;
}

/** Sol assignment counts; zeros for HL clients. */
export interface GatewayAssignments {
  presets: number;
  copytrade: number;
}

/** One row from the gateway's `GET /api/trading/clients` (lenient,
 * verified against the live `ClientSummary` contract). */
export interface GatewayClient {
  id: string;
  /** "hyperliquid" | "solana" */
  chain: string | null;
  wallet: string | null;
  label: string | null;
  paused: boolean | null;
  budget: GatewayBudget | null;
  active_config: GatewayActiveConfig | null;
  assignments: GatewayAssignments | null;
  open_positions: number | null;
  unrealized_pnl_usd: string | number | null;
  last_activity: string | null;
  created_at: string | null;
}

/** Budget update — any subset; `clear_*` flags null a field. */
export interface ClientBudgetReq {
  session_budget_usd?: string;
  max_position_usd?: string;
  default_size_usd?: string;
  session_budget_lamports?: number;
  per_trade_lamports?: number;
  clear_session_budget_usd?: boolean;
  clear_max_position_usd?: boolean;
  clear_default_size_usd?: boolean;
  clear_session_budget_lamports?: boolean;
  clear_per_trade_lamports?: boolean;
}

/** One Sol preset assignment on a client, name-resolved. */
export interface ClientPreset {
  preset_id: string;
  name: string;
  enabled: boolean;
  buy_size_lamports_override: number | null;
  ladder_override: LegSpec[] | null;
}

/** One Sol copy config bound to a client. */
export interface ClientCopyConfig {
  id: string;
  leader: string;
  label: string;
  enabled: boolean;
}

/** Merged local-vault + gateway view of one client wallet. */
export interface ClientInfo {
  /** Local vault wallet id, or `gw-<id>` for server-only rows. */
  id: string;
  chain: "sol" | "hl" | (string & {});
  address: string;
  label: string | null;
  /** Local per-client pause flag. */
  paused: boolean;
  primary: boolean;
  unlocked: boolean;
  /** `executor:<state>` | `standby[…]` | `locked` | `remote`. */
  runtime_state: string;
  runtime_detail: string | null;
  drift: string | null;
  gateway: GatewayClient | null;
}

export const ipc = {
  appVersion: () => invoke<string>("app_version"),
  /** Move the keystore aside (backup, never delete) so the app re-onboards.
   *  Backs the Unlock screen's "forgot passphrase / start fresh" action. */
  resetKeystore: () => invoke<string>("reset_keystore"),
  status: () => invoke<StatusReport>("signer_status"),
  setPaused: (paused: boolean) => invoke<void>("set_paused", { paused }),
  onboardingState: () => invoke<OnboardingState>("onboarding_state"),
  generateSolanaWallet: (password: string) =>
    invoke<{ pubkey: string }>("generate_solana_wallet", { password }),
  importSolanaWallet: (secret: string, password: string) =>
    invoke<{ pubkey: string }>("import_solana_wallet", { req: { secret, password } }),
  importHlKeystore: (privateKeyHex: string, password: string) =>
    invoke<{ address: string }>("import_hl_keystore", {
      req: { private_key_hex: privateKeyHex, password },
    }),
  unlock: (password: string, backend: "file" | "keychain") =>
    invoke<void>("unlock_keystores", { req: { password, backend } }),
  lock: () => invoke<void>("lock_keystores"),
  recentSigns: () => invoke<RecentSign[]>("list_recent_signs"),
  // Wallet management. `pubkey` selects a specific vault wallet
  // (default: the primary).
  exportSolKeystore: (dest: string, pubkey?: string) =>
    invoke<string>("export_sol_keystore", { dest, pubkey: pubkey ?? null }),
  revealSolSecret: (password: string, pubkey?: string) =>
    invoke<string>("reveal_sol_secret", { req: { password, pubkey: pubkey ?? null } }),
  removeSolKeystore: () => invoke<void>("remove_sol_keystore"),
  removeHlKeystore: () => invoke<void>("remove_hl_keystore"),
  // Multi-wallet clients.
  clientsList: () => invoke<ClientInfo[]>("clients_list"),
  clientAdd: (chain: "sol", password: string, label?: string) =>
    invoke<{ id: string; chain: string; address: string }>("client_add", {
      req: { chain, label: label ?? null, password },
    }),
  clientImport: (chain: "sol" | "hl", secret: string, password: string, label?: string) =>
    invoke<{ id: string; chain: string; address: string }>("client_import", {
      req: { chain, secret, label: label ?? null, password },
    }),
  /** Unlock + start the runtime for ONE freshly added wallet while the
   * app stays unlocked — without this the new client idles until the
   * next lock/unlock cycle. Idempotent for already-live ids. */
  clientActivate: (id: string, password: string) =>
    invoke<void>("client_activate", { id, password }),
  clientRemove: (id: string) => invoke<void>("client_remove", { id }),
  /** Delete a gateway-only registration (a remote `gw-…` row). Takes the
   * GATEWAY id (`gateway.id`). Server metadata only; keys on the device
   * that created the binding are untouched. */
  clientGatewayDeregister: (gatewayId: string) =>
    invoke<void>("client_gateway_deregister", { gatewayId }),
  clientLabel: (id: string, label: string | null) =>
    invoke<void>("client_label", { id, label }),
  clientPause: (id: string, paused: boolean) =>
    invoke<void>("client_pause", { id, paused }),
  clientSetPrimary: (id: string) => invoke<void>("client_set_primary", { id }),
  clientExportKeystore: (id: string, dest: string) =>
    invoke<string>("client_export_keystore", { id, dest }),
  // Per-client gateway config (all take the GATEWAY id — `gateway.id`).
  clientBudgetSet: (gatewayId: string, req: ClientBudgetReq) =>
    invoke<void>("client_budget_set", { gatewayId, req }),
  clientPresetsList: (gatewayId: string) =>
    invoke<ClientPreset[]>("client_presets_list", { gatewayId }),
  clientPresetAssign: (gatewayId: string, presetId: string, enabled: boolean) =>
    invoke<void>("client_preset_assign", { gatewayId, presetId, enabled }),
  clientPresetUnassign: (gatewayId: string, presetId: string) =>
    invoke<void>("client_preset_unassign", { gatewayId, presetId }),
  clientCopyConfigs: (gatewayId: string) =>
    invoke<ClientCopyConfig[]>("client_copy_configs", { gatewayId }),
  // Discord desktop auth.
  discordLoginStart: (serverUrl?: string) =>
    invoke<void>("discord_login_start", { serverUrl: serverUrl ?? null }),
  discordStatus: () => invoke<DiscordStatus>("discord_account_status"),
  discordUnlink: () => invoke<void>("discord_unlink"),
  /** Gateway access probe — see `AccessCheck`. */
  accessCheck: () => invoke<AccessCheck>("access_check"),
  // Hyperliquid surface.
  hlStatus: () => invoke<HlStatus>("hl_status"),
  hlPair: (
    serverUrl: string,
    token: string,
    accountAddress: string,
    totpCode?: string,
    agentAddress?: string,
  ) =>
    invoke<HlPairResult>("hl_pair", {
      req: {
        server_url: serverUrl,
        token,
        account_address: accountAddress,
        totp_code: totpCode ?? null,
        agent_address: agentAddress ?? null,
      },
    }),
  hlUnpair: () => invoke<void>("hl_unpair"),
  /** null = no pairing token yet, or the gateway predates the endpoint.
   * `clientId` selects a vault HL wallet's own pairing config/token —
   * compare the returned `agent_address` before claiming the state. */
  hlPairingStatus: (clientId?: string) =>
    invoke<HlPairingStatus | null>("hl_pairing_status", { clientId: clientId ?? null }),
  hlSetPaperMode: (paper: boolean) => invoke<void>("hl_set_paper_mode", { paper }),
  submitHlTotp: (code: string) => invoke<void>("submit_hl_totp", { code }),
  pickBackend: (backend: "file" | "keychain") =>
    invoke<void>("pick_backend", { backend }),
  openLogs: () => invoke<void>("open_logs"),
  openSetupUrl: (serverUrl: string) =>
    invoke<void>("open_setup_url", { req: { server_url: serverUrl } }),
  // Solana surface.
  solPositions: () => invoke<SolPosition[]>("sol_positions"),
  /** `pubkey` selects any vault wallet (default: the primary). */
  solBalance: (pubkey?: string) =>
    invoke<SolWalletBalance>("sol_wallet_balance", { pubkey: pubkey ?? null }),
  botPresets: () => invoke<BotPreset[]>("bot_presets_status"),
  copytradeConfigs: () => invoke<CopytradeConfig[]>("copytrade_configs"),
  solRuntimeStatus: () => invoke<SolRuntimeStatus>("sol_runtime_status"),
  solExecConfigGet: () => invoke<SolExecConfig>("sol_exec_config_get"),
  // NOTE: the former `sol_exec_config_set` (per-unlock session budget)
  // binding was removed in slice 9 — the setting retired with slice 8.
  /** Execution params (RPC / slippage / tip / submit mode). */
  solExecParamsSet: (params: SolExecParams) =>
    invoke<void>("sol_exec_params_set", { req: params }),
  detectCliKeystore: () => invoke<CliKeystoreInfo | null>("detect_cli_keystore"),
  importSolKeystoreFile: (path: string) =>
    invoke<CliKeystoreInfo>("import_sol_keystore_file", { path }),
  importExtensionKeystore: (json: string, password: string) =>
    invoke<{ pubkey: string }>("import_extension_keystore", { req: { json, password } }),
  // ─── Trade settings + position management (trade_actions.rs) ────
  // Sol copy-config CRUD (full field set).
  trackedWalletsList: () => invoke<TrackedWallet[]>("tracked_wallets_list"),
  trackedWalletSetCopyMode: (walletId: string, copyMode: boolean) =>
    invoke<void>("tracked_wallet_set_copy_mode", { walletId, copyMode }),
  solCopyConfigsFull: () => invoke<SolCopyConfigFull[]>("sol_copy_configs_full"),
  solCopyConfigCreate: (body: SolCopyConfigCreate) =>
    invoke<unknown>("sol_copy_config_create", { body }),
  solCopyConfigUpdate: (configId: string, patch: SolCopyConfigPatch) =>
    invoke<unknown>("sol_copy_config_update", { configId, patch }),
  solCopyConfigDelete: (configId: string) =>
    invoke<void>("sol_copy_config_delete", { configId }),
  // TP/SL ladders on Sol positions.
  solTargetsList: () => invoke<PositionTargetRow[]>("sol_targets_list"),
  solTargetArm: (mint: string, entryPriceUsd: string, legs: LegSpec[]) =>
    invoke<PositionTargetRow>("sol_target_arm", {
      mint,
      body: { entry_price_usd: entryPriceUsd, legs },
    }),
  solTargetDisarm: (mint: string) => invoke<void>("sol_target_disarm", { mint }),
  /** Sell `fractionBps` (1..=10000) of the HOLDING wallet's ON-CHAIN
   * balance through this device's signer engine (native routing +
   * Jupiter fallback). 10000 = sell everything actually held.
   * `ownerPubkey` = the position's attributed wallet (intents ledger);
   * the backend verifies it against real on-chain holdings and refuses
   * ambiguous routing instead of defaulting to the primary. */
  solPositionSell: (mint: string, fractionBps: number, ownerPubkey?: string | null) =>
    invoke<SellResult>("sol_position_sell", {
      mint,
      fractionBps,
      ownerPubkey: ownerPubkey ?? null,
    }),
  // Bot sessions — create/cancel the server row, arm/disarm THIS device.
  alphaPresets: () => invoke<PresetLite[]>("alpha_presets"),
  botSessionCreate: (body: CreateBotSessionReq) =>
    invoke<{ id: string }>("bot_session_create", { body }),
  botSessionCancel: (sessionId: string) =>
    invoke<void>("bot_session_cancel", { sessionId }),
  botArm: (req: BotArmReq) => invoke<void>("bot_arm", { req }),
  botDisarm: (sessionId?: string) =>
    invoke<void>("bot_disarm", { sessionId: sessionId ?? null }),
  botDeviceStatus: () => invoke<BotDeviceStatus>("bot_device_status"),
  // HL position management.
  hlClosePosition: (coin: string, percent: number) =>
    invoke<{ cloid: string; status: string }>("hl_close_position", { coin, percent }),
  hlPlaceTpsl: (
    coin: string,
    tpPrice: string | null,
    slPrice: string | null,
    closePercent?: number,
  ) =>
    invoke<{ cloids: string[]; status: string }>("hl_place_tpsl", {
      coin,
      tpPriceIn: tpPrice,
      slPriceIn: slPrice,
      closePercent: closePercent ?? null,
    }),
  // HL copy-config editing.
  hlCopyConfigsFull: () => invoke<HlCopyConfigFull[]>("hl_copy_configs_full"),
  hlCopyConfigCreate: (body: HlCopyConfigPatch & { target_wallet: string }) =>
    invoke<HlCopyConfigFull>("hl_copy_config_create", { body }),
  hlCopyConfigUpdate: (configId: string, patch: HlCopyConfigPatch) =>
    invoke<HlCopyConfigFull>("hl_copy_config_update", { configId, patch }),
  // Per-client preset-assignment overrides.
  clientPresetUpdate: (gatewayId: string, presetId: string, body: ClientPresetUpdateReq) =>
    invoke<void>("client_preset_update", { gatewayId, presetId, body }),
};

/** Discord CDN avatar URL for a linked account (null-safe). */
export function discordAvatarUrl(s: DiscordStatus): string | null {
  if (!s.avatar) return null;
  if (s.avatar.startsWith("http")) return s.avatar;
  if (!s.discord_id) return null;
  return `https://cdn.discordapp.com/avatars/${s.discord_id}/${s.avatar}.png?size=64`;
}
