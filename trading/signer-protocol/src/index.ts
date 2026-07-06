// The frozen 6-method contract between the web app and the local signer.
//
// Both signer-extension and signer-desktop implement this exact surface.
// Web app calls it via `window.degenbox.*` (extension injects it) or
// `fetch('http://127.0.0.1:5829/...')` (desktop daemon).

export type Base58 = string;
export type Lamports = number; // u64 — keep < 2^53 for JS

export interface ConnectResult {
  pubkey: Base58;
  balanceLamports: Lamports;
  walletLabel?: string;
}

export interface QuoteRequest {
  inputMint: Base58;
  outputMint: Base58;
  amountLamports: Lamports;
  slippageBps?: number; // default 500 (5 %)
}

export interface QuoteResult {
  routeId: string; // opaque; the signer caches the actual route
  expectedOutLamports: Lamports;
  minOutLamports: Lamports;
  priceImpactPct: number;
  feeLamports: Lamports;
}

export interface SwapRequest {
  routeId: string;
  /** override of the slippage from the quote, in bps */
  slippageBps?: number;
  /** Falcon tip in lamports — defaults to 1_000_000 (0.001 SOL) */
  tipLamports?: Lamports;
  /** When set, the signer skips creating a new `trading_intents` row and
   *  reuses this one for the `/submit` call. The web UI already POSTs an
   *  intent before calling swap() so it can render a pending state; the
   *  signer creating a second intent would orphan the first in `pending`
   *  forever. Optional for back-compat with callers that don't pre-create. */
  intentId?: string;
  /** ADDITIVE (v0.1+): hard floor on the output amount (raw base units) —
   *  the min-out the user actually saw on the quote card. Signers that
   *  honour it refuse the swap with HTTP 409 ("re-quote") when their
   *  route's effective min-out falls below the floor; older signers
   *  ignore the field and fall back to their own slippage-derived floor,
   *  so it is safe to send unconditionally. */
  minOutLamports?: Lamports;
}

export interface SwapResult {
  txSignature: Base58;
}

export interface BotEnableRequest {
  presetId: string;
  budgetLamports: Lamports;
  perTradeLamports: Lamports;
  expiresAtUnixMs: number;
  tipLamports?: Lamports;
  /** if provided, hard cap per single token */
  perTokenCapLamports?: Lamports;
}

export interface BotEnableResult {
  sessionId: string;
}

export interface SignerStatus {
  connected: boolean;
  pubkey?: Base58;
  activeBotSessions: Array<{
    sessionId: string;
    presetId: string;
    spentLamports: Lamports;
    budgetLamports: Lamports;
    expiresAtUnixMs: number;
  }>;
  /** ADDITIVE (v0.1+): semver of the local signer build. Older signers
   *  omit it — callers must treat absence as "legacy". */
  version?: string;
  /** ADDITIVE (v0.1+): which client serves this daemon. Known values:
   *  `"signer-app"` (Tauri desktop app), `"signer-cli"` (legacy CLI).
   *  Older signers omit it. */
  clientKind?: string;
}

/** The full RPC surface, mirrored 1:1 in extension and desktop. */
export interface SignerRpc {
  connect(): Promise<ConnectResult>;
  quote(req: QuoteRequest): Promise<QuoteResult>;
  swap(req: SwapRequest): Promise<SwapResult>;
  botEnable(req: BotEnableRequest): Promise<BotEnableResult>;
  botDisable(sessionId: string): Promise<void>;
  status(): Promise<SignerStatus>;
}
